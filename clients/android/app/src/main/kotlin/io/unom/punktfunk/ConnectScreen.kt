package io.unom.punktfunk

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.ContextCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.withStarted
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.Presence
import io.unom.punktfunk.kit.discovery.PresenceTracker
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.models.ActiveSession
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * How long a host's advertised actions stay fresh before this screen asks again — the desktop's
 * `pf_client_core::host_actions::TTL`. Long on purpose: what it governs (whether this device
 * holds the Host-power grant, whether the box can suspend) changes when an operator edits
 * access, not minute to minute, and every refresh is a TLS handshake against an idle host.
 */
private const val HOST_ACTIONS_TTL_MS = 300_000L

/**
 * The connect screen — discovery, trust and the dial itself, under either interface.
 *
 * The engine is a [ConnectController], remembered here for the screen's life: the dial and its
 * wake fallback, trust, request access and the `punktfunk://` router, with the state they share.
 * This function runs the effects that feed it (the browse, the probe sweep, host actions) and
 * the card rows only this screen has. What is drawn lives beside it — `buildHomeTiles`,
 * `ConnectGrid` and `ConnectPrompts` — each taking what it displays and handing back what was
 * pressed.
 */
@Composable
fun ConnectScreen(
    settings: Settings,
    onConnected: (ActiveSession) -> Unit,
    // Writes the global settings back: a speed test landing in the defaults layer
    // (design/client-settings-profiles.md §5.3), and the default host set, cleared or forgotten.
    onSettingsChange: (Settings) -> Unit = {},
    // (host, pinned preset id) — a pinned host+preset card opens ITS shelf, and the id is the
    // one-off every launch off that shelf runs with (design §5.2a). Null = the host's own tile.
    // Raised by "Browse library…" in a card's overflow.
    onOpenLibrary: (KnownHost, String?) -> Unit = { _, _ -> },
    // A `punktfunk://` URL to route (design/client-deep-links.md §3). This screen owns it because
    // it owns the connect path — trust decisions, the local-network grant, wake-and-retry — and a
    // link must go through all of them, not around them.
    deepLink: String? = null,
    onDeepLinkHandled: () -> Unit = {},
    // Whether ACCESS_LOCAL_NETWORK is held. App asks on entry and re-checks on resume, since the
    // console shell needs the grant too; this screen gates on it and re-asks from its banner.
    lnpGranted: Boolean,
    onAskLocalNetwork: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    val settingsNow = rememberUpdatedState(settings)
    val lnpNow = rememberUpdatedState(lnpGranted)
    val onConnectedNow = rememberUpdatedState(onConnected)
    val ctl = remember { ConnectController(context, scope, settingsNow, lnpNow, onConnectedNow) }
    val knownHostStore = ctl.knownHostStore
    val presetStore = ctl.presetStore
    val discovery = ctl.discovery
    var host by remember { mutableStateOf("") }
    var hostName by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("9777") }
    // The host streams at exactly this mode; "Native" settings resolve from the device display.
    val (w, h, hz) = settings.effectiveMode(context)

    // Back from the background the browse sat idle with its re-query interval doubling: ask again,
    // so returning to the screen is enough. Not on first entry, where ON_RESUME fires right after
    // the effect below starts the browse.
    DisposableEffect(Unit) {
        val lifecycle = (context as? LifecycleOwner)?.lifecycle
        var wasPaused = false
        val obs = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_PAUSE -> wasPaused = true
                Lifecycle.Event.ON_RESUME -> {
                    if (wasPaused) discovery.rescan()
                    wasPaused = false
                }
                else -> {}
            }
        }
        lifecycle?.addObserver(obs)
        onDispose { lifecycle?.removeObserver(obs) }
    }
    DisposableEffect(Unit) {
        ctl.resumeBrowse()
        onDispose { ctl.pauseBrowse() }
    }

    // The preset catalog as the cards draw it, re-read on entry: Settings may have changed it.
    var presets by remember { mutableStateOf(presetStore.all()) }
    // Learn wake MAC(s) from live adverts for hosts we've saved (parity with the desktop clients),
    // so we can Wake-on-LAN them once they sleep. Runs only when the discovered set changes; the
    // prefs write is guarded (no-op when unchanged), and we refresh the saved list only if a MAC
    // was actually newly learned.
    LaunchedEffect(ctl.discovered) {
        val discovered = ctl.discovered
        val learned = withContext(Dispatchers.IO) {
            var any = false
            // Matched the way the list de-dupes (fingerprint first), so a host advertising
            // from a new lease still teaches its record.
            val saved = knownHostStore.all()
            discovered.forEach { dh ->
                val kh = saved.firstOrNull { it.matches(dh) } ?: return@forEach
                if (dh.mac.isNotEmpty() && kh.mac != dh.mac) {
                    knownHostStore.learnMac(kh, dh.mac)
                    any = true
                }
                // Same for the OS-identity chain, so the card's icon survives the host sleeping.
                if (dh.os.isNotEmpty() && kh.os != dh.os) {
                    knownHostStore.learnOs(kh, dh.os)
                    any = true
                }
                // And the mgmt port, so a host that moved off 47990 keeps its library once this
                // device can no longer see the advert (VPN, routed subnet, multicast-dead Wi-Fi).
                val mgmt = dh.mgmtPort
                if (mgmt != null && kh.mgmtPort != mgmt) {
                    knownHostStore.learnMgmtPort(kh, mgmt)
                    any = true
                }
            }
            any
        }
        if (learned) ctl.refreshHosts()
    }
    // The probe sweep behind [ConnectController.reachable]. An mDNS advert is not proof of life: a
    // suspending host sends no goodbye, and its cache entry lives 75 minutes. [Presence] probes
    // every saved host at its live and saved address every ~12 s, and at once on a network change;
    // gated on LNP, since blocked UDP would only time out.
    val presence = remember { PresenceTracker() }
    var networkGen by remember { mutableIntStateOf(0) }
    DisposableEffect(Unit) {
        val onNetwork: () -> Unit = { networkGen++ }
        discovery.addNetworkListener(onNetwork)
        onDispose { discovery.removeNetworkListener(onNetwork) }
    }
    // Probe laps wait while the app is away: a stopped activity does not pause a coroutine.
    val appLifecycle = (context as? LifecycleOwner)?.lifecycle
    LaunchedEffect(ctl.savedHosts, lnpGranted, networkGen) {
        if (!lnpGranted) {
            ctl.reachable = emptySet()
            return@LaunchedEffect
        }
        while (true) {
            appLifecycle?.withStarted {}
            val saved = ctl.savedHosts
            val up = withContext(Dispatchers.IO) {
                Presence.sweep(
                    saved,
                    liveFor = { kh -> ctl.discovered.firstOrNull { kh.matches(it) } },
                    probe = { addr, port -> NativeBridge.nativeProbe(addr, port, Presence.PROBE_MS) },
                )
            }
            ctl.reachable = presence.apply(saved.map { it.id }.toSet(), up.keys)
            // A pinned host that answered somewhere else has moved: follow it, so the dial and
            // the library fetch go where it lives. The list refresh restarts this loop.
            val moved = withContext(Dispatchers.IO) {
                saved.any { kh -> up[kh.id]?.let { knownHostStore.learnAddress(kh.fpHex, it.address, it.port) } == true }
            }
            if (moved) {
                ctl.refreshHosts()
                return@LaunchedEffect
            }
            delay(12_000)
        }
    }
    LaunchedEffect(Unit) { ctl.loadIdentity() }
    // A saved host being edited (name / address / port / MAC).
    var editTarget by remember { mutableStateOf<KnownHost?>(null) }

    // What each paired host says this device may do TO it — sleep, restart, shut it down
    // (`design/host-actions.md` §7) — by fingerprint, with the moment we last asked.
    //
    // Learned on a slow TTL rather than when a menu opens: the row list has to be settled BEFORE
    // the menu draws, or rows would appear under a finger already on its way down, and two of
    // these rows end whatever is running on that machine. Empty for an older host (no such
    // route), an unreachable one, and any device without the grant — the menu simply has no
    // power rows then.
    var hostActions by remember { mutableStateOf<Map<String, List<HostActions.Action>>>(emptyMap()) }
    var hostActionsAt by remember { mutableStateOf<Map<String, Long>>(emptyMap()) }
    LaunchedEffect(ctl.savedHosts, ctl.identity) {
        val id = ctl.identity ?: return@LaunchedEffect
        while (true) {
            appLifecycle?.withStarted {}
            val now = android.os.SystemClock.elapsedRealtime()
            for (kh in ctl.savedHosts) {
                if (!kh.paired || kh.fpHex.isEmpty()) continue
                if (!kh.isOnline(ctl.reachable)) continue
                if (now - (hostActionsAt[kh.fpHex] ?: 0L) < HOST_ACTIONS_TTL_MS) continue
                // Stamp BEFORE the request, so a slow host cannot make every lap ask again.
                hostActionsAt = hostActionsAt + (kh.fpHex to now)
                val found = withContext(Dispatchers.IO) {
                    HostActions.list(id, kh.address, kh.effectiveMgmtPort, kh.fpHex)
                }
                hostActions = hostActions + (kh.fpHex to found)
            }
            delay(30_000)
        }
    }
    // The Switch profile picker a host menu opened; its answer is null while the host is asked.
    var switching by remember { mutableStateOf<Pair<KnownHost, ProfilesAnswer?>?>(null) }
    // A destructive host action awaiting its confirmation (restart / shut down).
    var confirmAction by remember { mutableStateOf<Pair<KnownHost, HostActions.Action>?>(null) }

    // Discovered hosts not already saved — a saved host (paired or TOFU) belongs in "Saved hosts",
    // not also in "Discovered", so we hide the overlap (matched by fingerprint when both carry it, so
    // it survives a DHCP address change; else by address:port). Mirrors the Apple client.
    val discoveredUnsaved = ctl.discovered.filter { dh -> ctl.savedHosts.none { it.matches(dh) } }

    // A speed test in flight: which host+preset it is measuring, and how far it has got. The
    // measurement is over a real connect, so it takes the same `connecting` gate every dial does.
    var speedTest by remember { mutableStateOf<HostCardEntry?>(null) }
    var speedTestPhase by remember { mutableStateOf<SpeedTestPhase>(SpeedTestPhase.Connecting) }

    fun startSpeedTest(entry: HostCardEntry) {
        val id = ctl.requireIdentity() ?: return
        // The magic packet isn't the only thing LNP blocks: without the grant this would EPERM its
        // way to a timeout and report a dead link on a perfectly good one.
        if (!lnpGranted) {
            ctl.lnpPrompt = true
            return
        }
        speedTest = entry
        speedTestPhase = SpeedTestPhase.Connecting
        ctl.notice = null
        ctl.connecting = true
        ctl.pauseBrowse() // a browse running through the burst would measure itself
        scope.launch {
            runSpeedTest(context, id, entry.host.address, entry.host.port, entry.host.fpHex) { p ->
                // A dismissed dialog abandons the run; don't drag it back onto the screen.
                if (speedTest != null) speedTestPhase = p
            }
            ctl.connecting = false
            ctl.resumeBrowse()
        }
    }

    fun togglePin(kh: KnownHost, preset: StreamPreset) {
        HostRecords.togglePin(knownHostStore, kh, preset.id)
        ctl.refreshHosts()
    }

    // "Copy link" — the self-emitted form every other client already hands out
    // (design/client-deep-links.md §4): the host's STABLE id first, with `host=` and `fp=` alongside,
    // so a link written today still lands on the right box after the host changes address or this
    // client is reinstalled. A PINNED card copies its own preset with it, because that combination
    // is the thing being copied; a host card copies no preset at all and so keeps honouring the
    // host's binding, exactly like a tap on it does.
    fun copyLink(kh: KnownHost, pin: StreamPreset?) {
        val url = DeepLinks.forHost(kh, preset = pin?.id).toUrl()
        val copied = putLinkOnClipboard(context, url)
        val message = linkCopyMessage(copied) ?: return
        // A success dressed as an error banner is a small lie: the notice line for a copy, the
        // status line for a failure.
        if (copied) ctl.notice = message else ctl.status = message
    }

    // Host actions (`design/host-actions.md` §7) — sleep, restart or shut the host down. The
    // menu rows come from what the HOST said it lets this device do, so a device without the
    // Host-power grant is offered none; a destructive one still asks first, because losing what
    // is running on that machine is not something a mis-tap should be able to do.
    fun runHostAction(kh: KnownHost, a: HostActions.Action) {
        val id = ctl.requireIdentity() ?: return
        val name = kh.name.ifBlank { kh.address }
        ctl.notice = "${a.label} — asking $name…"
        ctl.status = null
        // Whatever the host said about itself is about to be wrong: ask again next sweep.
        hostActionsAt = hostActionsAt - kh.fpHex
        scope.launch {
            ctl.notice = withContext(Dispatchers.IO) {
                HostActions.invoke(
                    id, kh.address, kh.effectiveMgmtPort, kh.fpHex, name, a.id, a.label,
                )
            }
        }
    }

    fun hostAction(kh: KnownHost, a: HostActions.Action) {
        when {
            // The host already said it cannot do this right now — say why, rather than send a
            // request we know it will refuse.
            !a.available ->
                ctl.notice = a.unavailableReason.ifEmpty { "${a.label} isn't available right now" }
            a.danger -> confirmAction = kh to a
            else -> runHostAction(kh, a)
        }
    }

    // Switch profile: ask the host, show the picker. A pick saves; it does not connect.
    fun switchProfile(kh: KnownHost) {
        val id = ctl.requireIdentity() ?: return
        switching = kh to null
        scope.launch {
            val answer = withContext(Dispatchers.IO) {
                HostProfiles.fetch(id, kh.address, kh.effectiveMgmtPort, kh.fpHex)
            }
            if (switching?.first?.id == kh.id) switching = kh to answer
        }
    }

    // "Send logs to host" — [SendLogs], the same upload the console's host menu runs. The outcome
    // is a notice either way (success and failure both name the host), because the row's whole job
    // is to tell a reporter whether the bundle actually landed.
    fun sendLogs(kh: KnownHost) {
        val id = ctl.requireIdentity() ?: return
        ctl.notice = "Sending logs to ${kh.name.ifBlank { kh.address }}…"
        ctl.status = null
        scope.launch {
            val message = withContext(Dispatchers.IO) { SendLogs.toHost(context, id, kh) }
            ctl.notice = message
        }
    }

    // A `punktfunk://` link, routed once the identity has landed: the effect reruns when it does.
    LaunchedEffect(deepLink, ctl.identity, ctl.savedHosts) {
        val url = deepLink ?: return@LaunchedEffect
        if (ctl.identity == null) return@LaunchedEffect
        onDeepLinkHandled()
        ctl.openLink(url)
    }

    var showManualSheet by remember { mutableStateOf(false) }

    fun forgetHost(kh: KnownHost) {
        HostRecords.forget(context, knownHostStore, kh, settings)?.let(onSettingsChange)
        ctl.refreshHosts()
    }

    /** Point the start-screen setting at [kh], or clear it. The caller persists. */
    fun setDefaultHost(kh: KnownHost, on: Boolean) {
        onSettingsChange(settings.copy(defaultHost = if (on) kh.id else null))
    }

    ConnectGrid(
        savedHosts = ctl.savedHosts,
        discovered = ctl.discovered,
        discoveredUnsaved = discoveredUnsaved,
        reachable = ctl.reachable,
        presets = presets,
        pinsFor = presetStore::pinsFor,
        connecting = ctl.connecting,
        notice = ctl.notice,
        status = ctl.status,
        lnpGranted = lnpGranted,
        onAskLocalNetwork = { ctl.lnpPrompt = true },
        onConnect = { kh, oneOff -> ctl.connect(kh.address, kh.port, oneOffPreset = oneOff, saved = kh) },
        onConnectDiscovered = { dh -> ctl.connect(dh.host, dh.port, dh) },
        onForget = { kh -> forgetHost(kh) },
        onEdit = { kh -> editTarget = kh },
        onWake = { kh -> ctl.wakeHost(kh) },
        onSpeedTest = { kh -> startSpeedTest(HostCardEntry(kh, null)) },
        onSendLogs = { kh -> sendLogs(kh) },
        onSwitchProfile = { kh -> switchProfile(kh) },
        hostActions = hostActions,
        onHostAction = { kh, a -> hostAction(kh, a) },
        onCopyLink = { kh, pin -> copyLink(kh, pin) },
        onTogglePin = { kh, p -> togglePin(kh, p) },
        onBrowseLibrary = { kh, pin -> onOpenLibrary(kh, pin?.id) },
        defaultHost = settings.defaultHost,
        onMakeDefault = { kh, on -> setDefaultHost(kh, on) },
        onRescan = { discovery.rescan() },
        onAddHost = { showManualSheet = true },
    )

    // Add Host stayed behind while the other modals moved into ConnectPrompts: its form fields are
    // remembered HERE, on purpose, so a half-typed address survives the sheet being dismissed and
    // reopened. Moving the block without moving that state would quietly change what a dismiss
    // costs; moving both is a separate decision from this one.
    if (showManualSheet) {
        AddHostSheet(
            hostName = hostName,
            onHostNameChange = { hostName = it },
            host = host,
            onHostChange = { host = it },
            port = port,
            onPortChange = { port = it },
            connecting = ctl.connecting,
            modeLabel = "$w×$h@$hz",
            onDismiss = { showManualSheet = false },
            onConnect = { h2, p, n -> ctl.connect(h2, p, manualName = n) },
        )
    }

    // Which layer a measurement would land in. Resolved here, not in the prompt: it is a question
    // for the preset store, and the Apply button and the caption above it must agree on the answer.
    val speedTestTarget = speedTest?.let { SpeedTestTarget.resolve(it.host, it.pin?.id, presetStore) }
    // Prefill a not-yet-learned MAC from the host's live advert, mirroring Apple's
    // `discovery.hosts.first { host.matches($0) }?.macAddresses`.
    val editSuggestedMacs =
        editTarget?.let { kh -> ctl.discovered.firstOrNull { kh.matches(it) }?.mac } ?: emptyList()

    ctl.profileAsk?.let { ask ->
        ProfilePickerDialog(
            hostName = ask.host.name.ifBlank { ask.host.address },
            answer = ProfilesAnswer.Listed(ask.listed),
            saved = ask.host.asProfile,
            gone = ask.gone,
            onPick = { ask.answer.complete(it) },
            onDismiss = { ask.answer.complete(null) },
        )
    }
    ctl.seatWait?.let { w ->
        SeatWaitDialog(w, onCancel = { w.cancelled.complete(Unit); ctl.seatWait = null })
    }
    switching?.let { (kh, answer) ->
        ProfilePickerDialog(
            hostName = kh.name.ifBlank { kh.address },
            answer = answer,
            saved = kh.asProfile,
            gone = null,
            onPick = { pick ->
                HostRecords.savePick(knownHostStore, kh, pick)
                ctl.refreshHosts()
                switching = null
            },
            onDismiss = { switching = null },
        )
    }

    // A destructive host action's confirmation. Kept here rather than in ConnectPrompts because
    // it is a one-question dialog owned by the row that raised it — the same place the row's
    // handler lives.
    confirmAction?.let { (kh, a) ->
        HostActionConfirmDialog(
            hostName = kh.name.ifBlank { kh.address },
            action = a,
            onConfirm = { confirmAction = null; runHostAction(kh, a) },
            onDismiss = { confirmAction = null },
        )
    }

    // Everything that floats above whichever home was drawn, in one place and in one order — see
    // ConnectPrompts.kt. It decides nothing: each action below lands back in the controller.
    ConnectPrompts(
        identity = ctl.identity,
        presets = presets,
        isOnline = { it.isOnline(ctl.reachable) },
        pendingTrust = ctl.pendingTrust,
        onPendingTrustChange = { ctl.pendingTrust = it },
        onTrustNew = ctl::trustNew,
        onPaired = ctl::paired,
        onRequestAccess = ctl::requestAccess,
        pendingLinkConnect = ctl.pendingLinkConnect,
        onConfirmLinkConnect = ctl::confirmLinkConnect,
        onDismissLinkConnect = { ctl.pendingLinkConnect = null },
        awaitingHostName = ctl.awaitingHostName,
        onCancelApproval = ctl::cancelApproval,
        speedTest = speedTest,
        speedTestTarget = speedTestTarget,
        speedTestPhase = speedTestPhase,
        onApplySpeedTest = { toPreset ->
            val done = speedTestPhase as? SpeedTestPhase.Done
            if (done != null && speedTestTarget != null) {
                val where = applySpeedTestResult(
                    done.recommendedKbps, speedTestTarget, toPreset, presetStore, settings,
                    onSettingsChange,
                )
                presets = presetStore.all()
                ctl.notice = "%.0f Mbit/s set in %s".format(done.recommendedMbps, where)
            }
            speedTest = null
        },
        onDismissSpeedTest = { speedTest = null },
        editTarget = editTarget,
        editSuggestedMacs = editSuggestedMacs,
        onSaveHost = { updated ->
            knownHostStore.save(updated)
            ctl.refreshHosts()
            editTarget = null
        },
        onDismissEdit = { editTarget = null },
        lnpPrompt = ctl.lnpPrompt,
        onAllowLocalNetwork = {
            ctl.lnpPrompt = false
            onAskLocalNetwork()
        },
        onOpenSystemSettings = {
            ctl.lnpPrompt = false
            context.startActivity(
                Intent(
                    android.provider.Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                    Uri.fromParts("package", context.packageName, null),
                ),
            )
        },
        onDismissLnpPrompt = { ctl.lnpPrompt = false },
        connectingHostName = ctl.connectingHostName,
        waker = ctl.waker,
        onCancelConnect = ctl::cancelConnect,
    )
}

/**
 * One entry in the saved-hosts grid: a host's own card ([pin] null), or one of its pinned
 * host+preset cards. Pins are additive presentation state on the host record — never duplicated
 * host entries, which would fork pairing, trust and renames (design §5.2a).
 */
internal data class HostCardEntry(val host: KnownHost, val pin: StreamPreset?) {
    val key: String get() = "card-${host.id}-${pin?.id ?: "primary"}"
}

/**
 * Whether NEARBY_WIFI_DEVICES is held (API 33+; not applicable below). We request it opportunistically
 * as a multicast-reception hedge on OEMs that filter multicast without it, but discovery (raw mDNS via
 * the native core + MulticastLock) does not depend on it.
 */
internal fun hasNearbyPermission(context: Context): Boolean =
    Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
        ContextCompat.checkSelfPermission(context, Manifest.permission.NEARBY_WIFI_DEVICES) ==
        PackageManager.PERMISSION_GRANTED

/**
 * Whether ACCESS_LOCAL_NETWORK is held (API 37+; below, the permission doesn't exist and local
 * network access is implicit). Android 17's Local Network Protection blocks ALL local-network
 * traffic for apps targeting SDK 37 without this runtime grant: UDP sends fail with EPERM, so the
 * QUIC dial surfaces as a silent handshake timeout and the mDNS browse receives nothing. Unlike
 * [hasNearbyPermission] this is load-bearing — nothing on the connect screen works without it.
 */
internal fun hasLocalNetworkPermission(context: Context): Boolean =
    Build.VERSION.SDK_INT < Build.VERSION_CODES.CINNAMON_BUN ||
        ContextCompat.checkSelfPermission(context, Manifest.permission.ACCESS_LOCAL_NETWORK) ==
        PackageManager.PERMISSION_GRANTED

/**
 * True when a saved host and a discovered advert are the same machine. Two known fingerprints
 * decide it alone — it survives a DHCP address change, and it keeps the other OS of a dual-boot
 * box (one lease, one MAC, a certificate each) out of the record already saved for the first.
 * Only when one side is unpinned does the address answer. Mirrors the Apple client's
 * `StoredHost.matches` and the Rust `discovery::same_host`; de-dupes "Discovered" against
 * "Saved hosts".
 */
internal fun KnownHost.matches(dh: DiscoveredHost): Boolean {
    val advFp = dh.fingerprint?.lowercase()
    if (!advFp.isNullOrEmpty() && fpHex.isNotEmpty()) return fpHex.lowercase() == advFp
    return address == dh.host && port == dh.port
}

/**
 * True when a saved host is reachable RIGHT NOW: it answered the last QUIC probe. Deliberately NOT
 * "advertising on mDNS" — an advert is a cache entry a sleeping host keeps alive for up to 75
 * minutes, so reading it as presence left the pip green and Wake-on-LAN silent for exactly the
 * machine that needed waking. It also never covered a routed host (Tailscale/VPN), which answers a
 * dial it never advertised for.
 *
 * `internal`, not private: the touch grid draws the same pip in its own file now, and the console's
 * tile builder is handed this as a lambda so it never has to know what "reachable" is made of.
 */
internal fun KnownHost.isOnline(reachable: Set<String>): Boolean = id in reachable
