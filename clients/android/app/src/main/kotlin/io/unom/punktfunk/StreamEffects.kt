package io.unom.punktfunk

import android.widget.Toast
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.LifecycleOwner
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.GamepadRouter
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.SessionAccess
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.RunningGame
import io.unom.punktfunk.kit.security.IdentityLoad
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.models.ActiveSession
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext

/**
 * The start-of-stream banner's two moments: [up] composes the pill at all, [alpha] runs it down
 * over the last 600 ms. A stream takes the whole screen and answers to none of the device's usual
 * gestures, so the banner says once how to get back out. The desktop console draws the same pill
 * for the same reason (`pf-console-ui/src/skia_overlay.rs`, BANNER_S = 6 s with a
 * BANNER_FADE_S = 0.6 s tail).
 */
internal class StartBanner(up: State<Boolean>, alpha: State<Float>) {
    val up by up
    val alpha by alpha
}

@Composable
internal fun rememberStartBanner(handle: Long): StartBanner {
    val up = remember(handle) { mutableStateOf(true) }
    var fading by remember(handle) { mutableStateOf(false) }
    val alpha = animateFloatAsState(
        targetValue = if (fading) 0f else 1f,
        // Linear, like the desktop's (BANNER_S - age) / BANNER_FADE_S ramp — Compose's default
        // easing would hold near-opaque and then drop, which reads as a glitch rather than a fade.
        animationSpec = tween(600, easing = LinearEasing),
        label = "streamStartBanner",
    )
    LaunchedEffect(handle) {
        delay(5400) // 6 s − the 0.6 s tail: fully opaque until here, exactly as on the desktop
        fading = true
        delay(600)
        up.value = false // stop composing it once it is invisible
    }
    return StartBanner(up, alpha)
}

/**
 * The decoder's picture size once known: a host framing the picture for this device (a join, a
 * mirrored head) sends a size other than the mode, and the placement follows the frames.
 */
@Composable
internal fun rememberDecodedSize(handle: Long): State<IntArray?> {
    val size = remember(handle) { mutableStateOf<IntArray?>(null) }
    LaunchedEffect(handle) {
        while (true) {
            NativeBridge.nativeVideoDecodedSize(handle)
                ?.takeIf { it.size >= 2 && it[0] > 0 && it[1] > 0 && !it.contentEquals(size.value) }
                ?.let { size.value = it }
            delay(500)
        }
    }
    return size
}

/**
 * The virtual controller's wire pad (design §4). While [shown] it holds one pad on the router, so
 * the host sees one controller arrive and, on hide, one leave (§9). Never toggled by the ring's own
 * open and close (§8 trap 4). Its kind follows the Controller type setting: Automatic is an Xbox
 * 360 pad, or a DualSense when this phone's gyro is to speak for it — a 360 has no motion plane.
 */
@Composable
internal fun rememberVirtualPad(
    handle: Long,
    shown: Boolean,
    settings: Settings,
    activity: MainActivity?,
): State<GamepadRouter.ExternalPad?> {
    val pad = remember(handle) { mutableStateOf<GamepadRouter.ExternalPad?>(null) }
    val kind = when {
        settings.gamepad != Gamepad.PREF_AUTO -> settings.gamepad
        settings.gyroOnPhone -> Gamepad.PREF_DUALSENSE
        else -> Gamepad.PREF_XBOX360
    }
    DisposableEffect(shown) {
        val ext = if (shown) {
            activity?.gamepadRouter?.openExternal(kind, ownMotion = false)
        } else {
            null
        }
        pad.value = ext
        onDispose {
            ext?.close()
            pad.value = null
        }
    }
    return pad
}

/**
 * The HUD's lines, polled once a second while [statsOn], and the same windows as numbers pushed
 * into [history] for the companion panel's graphs. Enabling resets the native window and the
 * history, so a re-show never renders stale data; switching off drops the last window for the
 * same reason. [tier] is read at each poll, so a tier change never blanks the numbers.
 */
@Composable
internal fun rememberStatsLines(
    session: ActiveSession,
    statsOn: Boolean,
    tier: StatsVerbosity,
    history: StatsHistory? = null,
): State<List<HudLine>> {
    val handle = session.handle
    val context = LocalContext.current
    val lines = remember { mutableStateOf<List<HudLine>>(emptyList()) }
    val pollTier by rememberUpdatedState(tier)
    LaunchedEffect(handle, statsOn) {
        NativeBridge.nativeSetVideoStatsEnabled(handle, statsOn)
        if (statsOn) {
            history?.clear()
            while (true) {
                delay(1000)
                // The panel's LIVE rate, re-read each poll: a governor that ignored the mode pin
                // leaves the panel below the stream, which the overlay names as a warning line.
                val display = runCatching { context.display }.getOrNull()
                lines.value = decodeHudLines(
                    NativeBridge.nativeVideoStatsLines(
                        handle, pollTier.ordinal, session.settings.advancedStats,
                        display?.refreshRate ?: 0f, display?.mode?.refreshRate ?: 0f,
                        session.presetName,
                    ),
                )
                if (history != null) StatsSample.of(NativeBridge.nativeVideoStatsSample(handle))?.let(history::push)
            }
        } else {
            lines.value = emptyList()
        }
    }
    return lines
}

/**
 * The 1 Hz session watch: the access level and its countdown, the operator's mute, and the end
 * of the session. A host that sleeps, crashes or drops off the network stops answering the QUIC
 * keep-alive, and the connection idle-times out (~8 s). The moment the session is dead this drops
 * back to the menu, so the user can wake the host instead of being stranded on a frozen picture.
 * The keep-alive holds a merely quiet connection open, so this fires only on a dead peer. Keyed on
 * [handle], so it stops the moment the screen goes (the handle is freed later, in onDispose).
 */
@Composable
internal fun SessionWatchEffect(
    handle: Long,
    initialAccess: IntArray?,
    ui: StreamUi,
    peripherals: StreamPeripherals,
    onSessionEnded: (SessionEndReason) -> Unit,
) {
    val context = LocalContext.current
    LaunchedEffect(handle) {
        var lastAccessSeq = initialAccess?.getOrNull(2) ?: 0
        while (true) {
            delay(1000)
            // Access first, ended second: a session about to close on its expiry gets its final
            // countdown read, which is what lets the ended branch word that close honestly.
            NativeBridge.nativeAccessState(handle)?.let { st ->
                val grants = st.getOrNull(0) ?: SessionAccess.ALL
                val seq = st.getOrNull(2) ?: 0
                if (grants != ui.accessGrants) {
                    ui.accessGrants = grants
                    peripherals.applyAccess(grants)
                }
                ui.accessRemaining = st.getOrNull(1) ?: 0
                if (seq != lastAccessSeq) {
                    lastAccessSeq = seq
                    // A fresh AccessUpdate close to the deadline is the host's T−5 m / T−1 m
                    // courtesy warning — surface it. Grant edits (and a warning's grant echo)
                    // otherwise just move the chip; a toast per edit would be noise.
                    if (ui.accessRemaining in 1..330) {
                        val mins = (ui.accessRemaining + 30) / 60
                        Toast.makeText(
                            context,
                            if (mins <= 1) {
                                "Access expires in about a minute."
                            } else {
                                "Access expires in about $mins minutes."
                            },
                            Toast.LENGTH_LONG,
                        ).show()
                    }
                }
            }
            // The operator's per-session mute rides the control stream, so the badge learns it
            // on this tick. The local bit is in the same mask — the toggle writes it at once.
            ui.audioMute = NativeBridge.nativeAudioMute(handle)
            if (NativeBridge.nativeSessionEnded(handle)) {
                // WHY it ended decides what the user is told: only a connection that actually
                // died says the host may be asleep. A quit game or a deliberate host end is not
                // a failure, and must not read like one.
                val reason = SessionEndReason.fromNative(NativeBridge.nativeEndReason(handle))
                when {
                    // Dying inside the countdown's final stretch IS the typed expiry close,
                    // recognized off the countdown because the end-reason byte has no expiry code.
                    ui.accessRemaining in 1..75 ->
                        Toast.makeText(
                            context,
                            "Your access to this host has expired.",
                            Toast.LENGTH_LONG,
                        ).show()
                    reason == SessionEndReason.LOST ->
                        Toast.makeText(
                            context,
                            "Connection lost — the host may be asleep. Wake it to reconnect.",
                            Toast.LENGTH_LONG,
                        ).show()
                    reason == SessionEndReason.HOST_ERROR ->
                        Toast.makeText(
                            context,
                            "The host ended the session with an error.",
                            Toast.LENGTH_LONG,
                        ).show()
                    // Deliberate endings — the player quit the game, the host was stopped, or we
                    // closed it. Leaving the stream IS the feedback; a toast would only add noise.
                    else -> {}
                }
                onSessionEnded(reason)
                return@LaunchedEffect
            }
        }
    }
}

/**
 * The host's actions, fetched at session start and every five minutes, never when the ring opens:
 * two of these buttons shut a machine down, and buttons that appear under a moving finger are a
 * hazard. Empty toward an older host, an unreachable one, or without [host]'s record.
 */
@Composable
internal fun rememberHostActions(handle: Long, host: KnownHost?): State<List<HostActions.Action>> {
    val context = LocalContext.current
    val actions = remember(handle) { mutableStateOf<List<HostActions.Action>>(emptyList()) }
    LaunchedEffect(handle) {
        val kh = host ?: return@LaunchedEffect
        if (kh.fpHex.isEmpty()) return@LaunchedEffect
        val identity = withContext(Dispatchers.IO) {
            (IdentityStore(context).load() as? IdentityLoad.Ok)?.identity
        } ?: return@LaunchedEffect
        while (true) {
            actions.value = withContext(Dispatchers.IO) {
                HostActions.list(identity, kh.address, kh.effectiveMgmtPort, kh.fpHex)
            }
            delay(300_000)
        }
    }
    return actions
}

/**
 * The game this device launched that this stream plays: the ring's End game. Read when the stream
 * starts and each time the ring opens ([ringOpen]); a failed read offers no End game.
 */
@Composable
internal fun rememberStreamedGame(handle: Long, host: KnownHost?, ringOpen: Boolean): State<RunningGame?> {
    val context = LocalContext.current
    val game = remember(handle) { mutableStateOf<RunningGame?>(null) }
    LaunchedEffect(handle, ringOpen) {
        val kh = host ?: return@LaunchedEffect
        if (kh.fpHex.isEmpty()) return@LaunchedEffect
        val identity = withContext(Dispatchers.IO) {
            (IdentityStore(context).load() as? IdentityLoad.Ok)?.identity
        } ?: return@LaunchedEffect
        game.value = withContext(Dispatchers.IO) {
            LibraryClient.fetchRunning(
                kh.address, kh.effectiveMgmtPort, identity.certPem, identity.privateKeyPem, kh.fpHex,
            )
        }.firstOrNull { it.streamedHere }
    }
    return game
}

/**
 * The background keep-alive (Settings › General): its ongoing notification, the leave-the-app
 * watch, and the away countdown. A null [keepAliveSpan] means leaving the app ends the session.
 * [endAway] ends it from a path that fires while the app is already away.
 */
@Composable
internal fun KeepAliveEffects(
    session: ActiveSession,
    host: KnownHost?,
    keepAliveSpan: Long?,
    endAway: (deliberate: Boolean) -> Unit,
    onSessionEnded: (SessionEndReason) -> Unit,
) {
    val handle = session.handle
    val context = LocalContext.current
    val keepAlive = keepAliveSpan != null
    var away by remember(handle) { mutableStateOf(false) }
    val startedAt = remember(handle) { System.currentTimeMillis() }
    // What the notification says. All of it is fixed at the handshake, so it is read once.
    val noteHost = host?.name?.takeIf { it.isNotEmpty() }
        ?: context.getString(R.string.app_name)
    val noteTitle = session.launchHold?.game?.title
    val modeLine = remember(handle) {
        val mode = NativeBridge.nativeVideoSize(handle)
        val w = mode?.getOrNull(0) ?: 0
        val h = mode?.getOrNull(1) ?: 0
        val hz = mode?.getOrNull(2) ?: 0
        listOfNotNull(
            if (w > 0 && h > 0) "$w×$h" else null,
            if (hz > 0) "$hz Hz" else null,
            NativeBridge.nativeVideoCodecLabel(handle).takeIf { it.isNotEmpty() },
        ).joinToString(" · ")
    }
    fun note(deadline: Long?) = StreamNote(noteHost, noteTitle, modeLine, startedAt, deadline)

    // The notification goes up when the session starts: an app already in the background may
    // not start a foreground service, and without it the OS freezes the process, audio and QUIC
    // traffic with it. Its End action is the ring's, a deliberate quit.
    DisposableEffect(handle, keepAlive) {
        if (keepAlive) {
            StreamKeepAliveService.onEnd = { endAway(true) }
            StreamKeepAliveService.start(context, note(null))
        }
        onDispose {
            StreamKeepAliveService.onEnd = null
            StreamKeepAliveService.stop(context)
        }
    }

    // Leaving the app (Home, task switch, screen off). Android does not suspend a backgrounded
    // process, so without the keep-alive this must end the session or the host holds it for
    // nobody. `onSessionEnded` runs the one real teardown; not a quit, so the host lingers the
    // display and coming straight back is a fast reconnect.
    DisposableEffect(handle, keepAlive) {
        val lifecycle = (context as? LifecycleOwner)?.lifecycle
        val obs = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_STOP -> if (keepAlive) away = true else onSessionEnded(SessionEndReason.LOCAL)
                Lifecycle.Event.ON_START -> away = false
                else -> {}
            }
        }
        lifecycle?.addObserver(obs)
        onDispose { lifecycle?.removeObserver(obs) }
    }

    // While away the notification counts down, and the session gets exactly that long: a host
    // can't tell a player who walked off from one who is watching. Coming back re-keys this
    // effect, which cancels the wait. The give-up is not a quit either.
    LaunchedEffect(handle, keepAliveSpan, away) {
        val span = keepAliveSpan ?: return@LaunchedEffect
        if (!away) {
            StreamKeepAliveService.update(context, note(null))
            return@LaunchedEffect
        }
        StreamKeepAliveService.update(context, note(System.currentTimeMillis() + span))
        delay(span)
        endAway(false)
    }
}
