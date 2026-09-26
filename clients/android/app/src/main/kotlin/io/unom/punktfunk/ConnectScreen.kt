package io.unom.punktfunk

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.util.Log
import android.widget.Toast
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
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.discovery.Presence
import io.unom.punktfunk.kit.discovery.PresenceTracker
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.HostResolution
import io.unom.punktfunk.kit.link.LinkError
import io.unom.punktfunk.kit.link.LinkRoute
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IDENTITY_OBTAIN_TIMEOUT_MS
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.security.obtainIdentity
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.models.PendingLinkConnect
import io.unom.punktfunk.models.PendingTrust
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Handshake budget for the no-PIN "request access" connect. Must exceed the host's approval-park
 * window (~180 s) so a slow operator approval still lands on this same parked connection rather than
 * timing the client out first. Mirrors the Linux client's 185 s.
 */
private const val REQUEST_ACCESS_TIMEOUT_MS = 185_000

private const val TAG = "pf.connect"

// A failed identity load's line — distinct from "not ready", which implies a retry nothing ran.
private const val IDENTITY_UNAVAILABLE =
    "Couldn't create an identity — this device's secure key storage isn't working"

/**
 * How long a host's advertised actions stay fresh before this screen asks again — the desktop's
 * `pf_client_core::host_actions::TTL`. Long on purpose: what it governs (whether this device
 * holds the Host-power grant, whether the box can suspend) changes when an operator edits
 * access, not minute to minute, and every refresh is a TLS handshake against an idle host.
 */
private const val HOST_ACTIONS_TTL_MS = 300_000L

/**
 * A no-PIN "request access" connect in flight — the host being requested (drives the cancelable
 * "Waiting for approval…" dialog) and a per-attempt flag the Cancel button trips. The connect is a
 * blocking call with no abort, so Cancel returns the UI immediately and a late result checks
 * [cancelled] and tears the (possibly just-approved) session down silently rather than navigating.
 */
private class RequestAccessState(val target: PendingTrust) {
    val cancelled = AtomicBoolean(false)
}

/**
 * A plain dial in flight — [hostName] labels the unified [ConnectOverlay]'s "Connecting…" phase, and
 * [cancelled] lets its Cancel abort. The native connect is a blocking call with no abort, so Cancel
 * returns the UI immediately and a late-arriving handle is torn down silently rather than navigating
 * into a session the user already backed out of. Mirrors [RequestAccessState]'s late-result handling.
 */
private class ConnectAttempt(val hostName: String) {
    val cancelled = AtomicBoolean(false)
}

/**
 * The connect screen — discovery, trust and the dial itself, under either interface.
 *
 * What is left in this file is the STATE and the engine: the mDNS browse and the permission that
 * gates it, the identity, the host and preset stores, the trust decision, the dial and its wake
 * fallback, and the `punktfunk://` router. What was drawn from that state now lives beside it —
 * `buildHomeTiles` (the console carousel's contents), `ConnectGrid` (the touch home) and
 * `ConnectPrompts` (everything modal, plus the connect takeover). They hold no state of their own,
 * which is why they could leave: each one takes what it displays and hands back what was pressed.
 *
 * The engine did NOT leave, and shouldn't until it has somewhere to live: it closes over ~20 locals
 * that a dozen callbacks read and write, and hoisting it means inventing a state holder — a second
 * refactor, and a second thing to get wrong.
 */
@Composable
fun ConnectScreen(
    settings: Settings,
    onConnected: (ActiveSession) -> Unit,
    // Writes the global defaults back. Only the speed test uses it — that is the one action on this
    // screen that can land in the defaults layer (design/client-settings-profiles.md §5.3).
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
    var host by remember { mutableStateOf("") }
    var hostName by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("9777") }
    var connecting by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf<String?>(null) }
    // A confirmation, as opposed to [status]'s failures — "75 Mbit/s set in “Travel”". Separate
    // state because the two read completely differently: an error banner is red on purpose, and a
    // successful write dressed as one is a small lie every time it appears.
    var notice by remember { mutableStateOf<String?>(null) }
    // A plain dial in flight (drives the "Connecting…" phase of the full-screen ConnectOverlay); null
    // when idle or when the request-access / wake flows own the screen instead.
    var attempt by remember { mutableStateOf<ConnectAttempt?>(null) }
    // The host streams at exactly this mode; "Native" settings resolve from the device display.
    val (w, h, hz) = settings.effectiveMode(context)

    // mDNS discovery scoped to this screen, via the native mdns-sd browse (HostDiscovery) — its
    // onChange fires on the main thread, so it can set Compose state directly. The grants it needs
    // are asked in App, above both shells; `lnpGranted` here only gates what would otherwise EPERM
    // its way to a timeout, and a denial shows as the grid's banner.
    val discovery = remember { HostDiscovery.shared(context) }
    val discoveredState = remember { mutableStateOf<List<DiscoveredHost>>(emptyList()) }
    val discovered by discoveredState
    // One value, because subscribing IS what runs the browse: the pauses below (a dial, a wake, a
    // speed test) drop this exact subscriber and the resumes hand back the same one.
    val subscriber = remember { { hosts: List<DiscoveredHost> -> discoveredState.value = hosts } }
    // The rationale dialog: raised by the banner, and by a dial or wake attempted without the grant.
    var lnpPrompt by remember { mutableStateOf(false) }
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
        discovery.addListener(subscriber)
        onDispose { discovery.removeListener(subscriber) }
    }

    val identityStore = remember { IdentityStore(context) }
    val knownHostStore = remember { KnownHostStore(context) }
    var savedHosts by remember { mutableStateOf(knownHostStore.all()) }
    // The settings-preset catalog. Read here (not in the settings screen's copy) because this is
    // where presets are USED: to resolve what a tap connects with, to offer the one-offs, and to
    // render the pinned cards. Re-read on entry, since Settings may have changed it in between.
    val presetStore = remember { PresetStore(context) }
    var presets by remember { mutableStateOf(presetStore.all()) }
    // Wakes a sleeping saved host and waits for it to reappear on mDNS before dialing (its overlay
    // rides over both the touch and console home). Fire-and-forget WoL isn't enough — a cold boot can
    // take a minute-plus to advertise again.
    val waker = remember { WakeController(scope) }
    // Learn wake MAC(s) from live adverts for hosts we've saved (parity with the desktop clients),
    // so we can Wake-on-LAN them once they sleep. Runs only when the discovered set changes; the
    // prefs write is guarded (no-op when unchanged), and we refresh the saved list only if a MAC
    // was actually newly learned.
    LaunchedEffect(discovered) {
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
        if (learned) savedHosts = knownHostStore.all()
    }
    // Saved hosts proven reachable by a QUIC probe, by record id — and the ONLY thing [isOnline]
    // reads. An mDNS advert is not proof of life: it is a cache entry with a 75-minute TTL that a
    // host suspending sends no goodbye for, so a sleeping machine kept every pip green and every
    // "not advertising" wake gate shut. [Presence] probes every saved host at its live address
    // and its saved one, off the main thread, every ~12 s, and at once when the device lands on
    // another network; gated on LNP (blocked UDP would just time out). `rememberUpdatedState`
    // keeps the 1 Hz mDNS updates from restarting the loop.
    var reachable by remember { mutableStateOf<Set<String>>(emptySet()) }
    val presence = remember { PresenceTracker() }
    val discoveredNow by rememberUpdatedState(discovered)
    var networkGen by remember { mutableIntStateOf(0) }
    DisposableEffect(Unit) {
        val onNetwork: () -> Unit = { networkGen++ }
        discovery.addNetworkListener(onNetwork)
        onDispose { discovery.removeNetworkListener(onNetwork) }
    }
    LaunchedEffect(savedHosts, lnpGranted, networkGen) {
        if (!lnpGranted) {
            reachable = emptySet()
            return@LaunchedEffect
        }
        while (true) {
            val saved = savedHosts
            val up = withContext(Dispatchers.IO) {
                Presence.sweep(
                    saved,
                    liveFor = { kh -> discoveredNow.firstOrNull { kh.matches(it) } },
                    probe = { addr, port -> NativeBridge.nativeProbe(addr, port, Presence.PROBE_MS) },
                )
            }
            reachable = presence.apply(saved.map { it.id }.toSet(), up.keys)
            // A pinned host that answered somewhere else has moved: follow it, so the dial and
            // the library fetch go where it lives. The list refresh restarts this loop.
            val moved = withContext(Dispatchers.IO) {
                saved.any { kh -> up[kh.id]?.let { knownHostStore.learnAddress(kh.fpHex, it.address, it.port) } == true }
            }
            if (moved) {
                savedHosts = knownHostStore.all()
                return@LaunchedEffect
            }
            delay(12_000)
        }
    }
    // Mint-once on genuine first run; an Unrecoverable store (decrypt failure) surfaces here and
    // refuses to connect — never silently shadow-minting a new identity (which would force re-pair).
    // Tri-state — in flight / ready / failed — because a tap on any guarded action doubles as the
    // retry: a failed load keeps its own message instead of the "not ready" line that claims one.
    var identity by remember { mutableStateOf<ClientIdentity?>(null) }
    var identityFailed by remember { mutableStateOf(false) }
    var identityLoading by remember { mutableStateOf(false) }
    fun loadIdentity() {
        if (identity != null || identityLoading) return
        identityLoading = true
        scope.launch {
            var threw = false
            val loaded = withTimeoutOrNull(IDENTITY_OBTAIN_TIMEOUT_MS) {
                withContext(Dispatchers.IO) {
                    runCatching { obtainIdentity(identityStore) }
                        .onFailure { threw = true; Log.w(TAG, "identity unavailable", it) }
                        .getOrNull()
                }
            }
            if (loaded == null && !threw) Log.w(TAG, "identity obtain timed out")
            identityLoading = false
            identity = loaded
            identityFailed = loaded == null
            if (identityFailed) status = IDENTITY_UNAVAILABLE
        }
    }
    // Every identity-gated action funnels here: ready → the identity; in flight → "not ready";
    // failed → the real failure and a fresh attempt, so the reporting tap is also its retry.
    fun requireIdentity(): ClientIdentity? {
        identity?.let { return it }
        if (identityFailed) {
            status = IDENTITY_UNAVAILABLE
            loadIdentity()
        } else {
            status = "Identity not ready yet — try again in a moment"
        }
        return null
    }
    LaunchedEffect(Unit) { loadIdentity() }
    // A trust decision awaiting the user (first-connect TOFU / fp changed / PIN pairing / the
    // request-access-or-PIN choice).
    var pendingTrust by remember { mutableStateOf<PendingTrust?>(null) }
    // A `punktfunk://` link that named a saved host by a guessable reference, awaiting the OK.
    var pendingLinkConnect by remember { mutableStateOf<PendingLinkConnect?>(null) }
    // A no-PIN "request access" connect in flight (the cancelable "Waiting for approval…" dialog).
    var awaiting by remember { mutableStateOf<RequestAccessState?>(null) }
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
    val reachableNow by rememberUpdatedState(reachable)
    LaunchedEffect(savedHosts, identity) {
        val id = identity ?: return@LaunchedEffect
        while (true) {
            val now = android.os.SystemClock.elapsedRealtime()
            for (kh in savedHosts) {
                if (!kh.paired || kh.fpHex.isEmpty()) continue
                if (!kh.isOnline(reachableNow)) continue
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
    // A destructive host action awaiting its confirmation (restart / shut down).
    var confirmAction by remember { mutableStateOf<Pair<KnownHost, HostActions.Action>?>(null) }

    // Discovered hosts not already saved — a saved host (paired or TOFU) belongs in "Saved hosts",
    // not also in "Discovered", so we hide the overlap (matched by fingerprint when both carry it, so
    // it survives a DHCP address change; else by address:port). Mirrors the Apple client.
    val discoveredUnsaved = discovered.filter { dh -> savedHosts.none { it.matches(dh) } }

    // Issue the native connect (shared by the normal connect and the request-access path). A plain
    // desktop connect (no library launch) — the library launcher calls [connectToHost] with an id.
    suspend fun connectNative(
        id: ClientIdentity,
        targetHost: String,
        targetPort: Int,
        pinHex: String,
        timeoutMs: Int,
        preset: StreamPreset?,
        launch: String?,
    ): Long = connectToHost(
        context, settings.effectiveFor(preset), id, targetHost, targetPort, pinHex,
        launch = launch, dialer = "touch/host-grid", timeoutMs = timeoutMs, preset = preset,
    )

    // What the stream screen is handed: the settings this connect actually used, plus the HOST's
    // clipboard decision (a property of the record, not a global). A host we never saved — a
    // connect that failed to pin — gets the secure default: no clipboard until the user enables
    // it for that host (security-review 2026-08-31 M-8).
    fun session(handle: Long, record: KnownHost?, preset: StreamPreset?): ActiveSession {
        // The session's own Welcome carries where this host serves its library. Save it now: this
        // is the only source that does not need an mDNS advert, so it is what makes a host that
        // moved off 47990 browsable over a VPN or when it was added by address. 0 = not
        // advertised, and learnMgmtPort ignores it.
        if (record != null) {
            NativeBridge.nativeHostMgmtPort(handle).takeIf { it > 0 }?.let {
                knownHostStore.learnMgmtPort(record, it)
            }
        }
        return ActiveSession(
            handle,
            settings.effectiveFor(preset),
            clipboardSync = record?.clipboardSync ?: false,
            presetName = preset?.name,
            hostId = record?.id,
        )
    }

    // The actual dial (identity already ready). A TOFU dial (pinHex null) pins what the host
    // presented, as an unpaired known host. [onFailure] takes over an unreachable dial (the
    // wake-wait fallback, discovery already restarted); [onMismatch] takes over a refused pin.
    fun doConnectDirect(
        targetHost: String,
        targetPort: Int,
        name: String,
        pinHex: String?,
        preset: StreamPreset?,
        launch: String? = null,
        onFailure: (() -> Unit)? = null,
        onMismatch: (() -> Unit)? = null,
    ) {
        val id = requireIdentity() ?: return
        val thisAttempt = ConnectAttempt(name)
        attempt = thisAttempt // shows the ConnectOverlay's "Connecting…" phase immediately
        connecting = true
        status = null
        notice = null
        discovery.removeListener(subscriber) // let the browse go; the stream session wants the radio
        scope.launch {
            val handle =
                connectNative(id, targetHost, targetPort, pinHex ?: "", CONNECT_TIMEOUT_MS, preset, launch)
            // Cancelled mid-dial: the UI's already been returned (and discovery restarted) by
            // cancelConnect — drop the just-opened session silently rather than navigating into it.
            if (thisAttempt.cancelled.get()) {
                if (handle != 0L) withContext(Dispatchers.IO) { NativeBridge.nativeClose(handle) }
                return@launch
            }
            attempt = null
            connecting = false
            if (handle != 0L) {
                // By this dial's pin: the address may also name the other OS of a dual-boot box.
                var record = pinHex?.let { knownHostStore.resolve(it, targetHost, targetPort) }
                if (pinHex == null) { // TOFU: pin what we observed (unpaired)
                    val fp = NativeBridge.nativeHostFingerprint(handle)
                    if (fp.isNotEmpty()) {
                        record = knownHostStore.trust(targetHost, targetPort, name, fp, paired = false)
                    }
                }
                onConnected(session(handle, record, preset))
            } else {
                discovery.addListener(subscriber)
                val token = NativeBridge.nativeTakeLastError()
                val unreachable = token == "timeout" || token == "io" || token.isEmpty()
                if (onFailure != null && unreachable) {
                    // Unreachable — hand off to the wake-and-wait flow — clearing `attempt` above
                    // and setting `waker.waking` here land in one recompose, so the overlay slides
                    // Connecting → Waking without a blank frame.
                    onFailure()
                } else if (onMismatch != null && token == "crypto") {
                    // The saved pin was refused: another identity answers at this address.
                    onMismatch()
                } else {
                    // A typed host rejection (busy / versions differ / pairing required) means the
                    // host is awake — waking it would be nonsense; show the stated reason instead.
                    status = ConnectErrors.connectMessage(token, requestAccess = false)
                }
            }
        }
    }

    // Cancel a plain dial in flight (the overlay's "Connecting…" phase, B / Cancel). The native
    // connect can't be aborted, so flag this attempt (a late handle is closed silently in
    // doConnectDirect) and return the UI now, resuming the discovery we paused for the dial.
    fun cancelConnect() {
        attempt?.cancelled?.set(true)
        attempt = null
        connecting = false
        discovery.addListener(subscriber)
    }

    // Wake-aware connect. If auto-wake is on (Settings.autoWakeEnabled) and the target is a saved
    // host with a learned MAC that the probe did NOT reach, fire a wake packet and DIAL IMMEDIATELY
    // — looking unreachable does not mean unreachable (a host over a routed network —
    // Tailscale/VPN/another subnet — answers a dial it never advertised for, and gating the dial on
    // presence bricked exactly those reconnects). A genuinely-asleep box is already booting while
    // the dial times out; only a FAILED dial falls into the wake-and-wait flow (WakeController's
    // "Waking…" overlay), which redials once the host answers. Otherwise (auto-wake off, no MAC, or
    // already reachable) dial straight through.
    fun doConnect(
        targetHost: String,
        targetPort: Int,
        name: String,
        pinHex: String?,
        oneOffPreset: String?,
        launch: String? = null,
        onMismatch: (() -> Unit)? = null,
    ) {
        if (requireIdentity() == null) return
        // The record this dial's pin names. A TOFU dial is to a host not saved yet, so only a
        // placeholder can be it — never the other OS of a dual-boot box at the same address.
        val kh = knownHostStore.resolve(pinHex ?: "", targetHost, targetPort)
        // Latched here, not per dial attempt: a wake-and-redial must stream with the same preset
        // the user asked for, and the "applies from the next session" footers stay truthful.
        val preset = presetStore.resolveFor(kh, oneOffPreset)
        val macs = kh?.mac ?: emptyList()
        // "Up" = a live advert that is THIS host — matched by fingerprint first (so it survives a DHCP
        // address change on a cold boot), else by address:port. Returns the CURRENT advert so we can
        // dial its live address rather than the stale saved one.
        fun liveAdvert(): DiscoveredHost? =
            if (kh != null) discovered.firstOrNull { kh.matches(it) }
            else discovered.firstOrNull { it.host == targetHost && it.port == targetPort }
        val down = kh == null || !kh.isOnline(reachable)
        if (settings.autoWakeEnabled && macs.isNotEmpty() && down) {
            // Fire-and-forget first packet (harmless if it's awake), then dial-first.
            scope.launch(Dispatchers.IO) { NativeBridge.nativeWakeOnLan(macs.joinToString(","), targetHost) }
            doConnectDirect(targetHost, targetPort, name, pinHex, preset, launch, onMismatch = onMismatch, onFailure = {
                waker.start(
                    hostName = name,
                    connectsAfter = true,
                    macs = macs,
                    lastIp = targetHost,
                    // A live advert would answer in milliseconds and lie (see [isOnline]); this
                    // asks the host itself, at the address it advertises if it moved lease.
                    isOnline = {
                        val live = liveAdvert()
                        withContext(Dispatchers.IO) {
                            Presence.isSelf(
                                pinHex ?: "",
                                NativeBridge.nativeProbe(
                                    live?.host ?: targetHost, live?.port ?: targetPort, 3_000,
                                ),
                            )
                        }
                    },
                    onOnline = {
                        val live = liveAdvert()
                        // Woke back on a new address? Re-point the saved record at it, keeping
                        // the one it left, then dial there (no fallback on this redial — a
                        // second failure surfaces as the plain error).
                        if (live != null && kh != null && knownHostStore.learnAddress(kh.fpHex, live.host, live.port)) {
                            savedHosts = knownHostStore.all()
                        }
                        doConnectDirect(
                            live?.host ?: targetHost, live?.port ?: targetPort, name, pinHex,
                            preset, launch, onMismatch = onMismatch,
                        )
                    },
                )
            })
        } else {
            doConnectDirect(targetHost, targetPort, name, pinHex, preset, launch, onMismatch = onMismatch)
        }
    }

    // The no-PIN "request access" path (delegated approval): open a normal identified connect that
    // the host PARKS until the operator clicks Approve in its console/web UI, showing a cancelable
    // "Waiting for approval…" dialog meanwhile. The SAME connection is admitted on approval (no
    // reconnect), so on success we record the host as PAIRED — the operator's approval IS the pairing.
    // The connect can't be aborted, so Cancel returns the UI immediately and a late result is torn
    // down silently via the per-attempt flag (mirrors the Linux client's request-access flow).
    fun requestAccess(target: PendingTrust) {
        val id = requireIdentity() ?: return
        val req = RequestAccessState(target)
        awaiting = req
        connecting = true
        status = null
        discovery.removeListener(subscriber) // same, for the session parked behind the console hold
        scope.launch {
            // Pin the advertised fingerprint for a discovered host (defence against an impostor while
            // we wait); a manually-typed host has none, so trust-on-first-use.
            val pinHex = target.advertisedFp ?: ""
            // A host being trusted for the first time can't have a binding yet, so this is always
            // the plain defaults — a preset only ever enters via a later, deliberate choice.
            val handle = connectNative(
                id, target.host, target.port, pinHex, REQUEST_ACCESS_TIMEOUT_MS,
                preset = null, launch = target.launch,
            )
            // Cancelled while we were parked: tear the (possibly just-approved) session down and
            // don't touch UI a fresh action may now own.
            if (req.cancelled.get()) {
                if (handle != 0L) withContext(Dispatchers.IO) { NativeBridge.nativeClose(handle) }
                return@launch
            }
            awaiting = null
            connecting = false
            if (handle != 0L) {
                // Approved — save the host as PAIRED, pinning the fingerprint it presented, so
                // future connects are silent (exactly like after a PIN ceremony).
                val fp = NativeBridge.nativeHostFingerprint(handle)
                var record = knownHostStore.resolve(fp, target.host, target.port)
                if (fp.isNotEmpty()) {
                    record = knownHostStore.trust(target.host, target.port, target.name, fp, paired = true)
                    savedHosts = knownHostStore.all()
                }
                onConnected(session(handle, record, preset = null))
            } else {
                // Cause-specific: an operator denial, an approval timeout, and a request that
                // never reached the host are different problems with different fixes.
                status = ConnectErrors.connectMessage(
                    NativeBridge.nativeTakeLastError(),
                    requestAccess = true,
                )
                discovery.addListener(subscriber)
            }
        }
    }

    // Decide pinned-reconnect vs TOFU vs pairing before connecting. The record is the tapped card's,
    // else the one the advertised pin names, else what a typed address answers with — never a record
    // pinned to another fingerprint (both OS installs of a dual-boot box answer at one lease). TOFU
    // only when the host advertised pair=optional; otherwise request access or the PIN ceremony.
    fun connect(
        targetHost: String,
        targetPort: Int,
        dh: DiscoveredHost? = null,
        manualName: String? = null,
        // A one-off "Connect with ▸" pick. `null` = follow the host's binding (a plain tap);
        // `""` = force the global defaults, which is a real choice on a bound host and must
        // therefore survive as a value rather than collapsing into "unset". NEVER rebinds.
        oneOffPreset: String? = null,
        // A library id the host should boot straight into (`launch=` on a link).
        launch: String? = null,
        // The saved card this dial came from: its record decides, not its address.
        saved: KnownHost? = null,
    ) {
        // Every dial/pair path funnels through here — with local network access denied the connect
        // can only EPERM its way to a 10 s timeout, so ask instead of pretending to try.
        if (!lnpGranted) {
            lnpPrompt = true
            return
        }
        val adv = dh?.fingerprint?.lowercase()
        val known = if (saved != null) {
            knownHostStore.byId(saved.id)
        } else {
            knownHostStore.resolve(adv, targetHost, targetPort)
        }
        val typed = manualName?.trim()?.takeIf { it.isNotEmpty() }
        // Label precedence: a saved host keeps its (possibly user-renamed) name; else the discovered
        // mDNS name; else the name typed in the Add-host sheet; else the bare address.
        val name = known?.name ?: dh?.name ?: typed ?: targetHost
        when {
            // A pinned record → silent pinned reconnect; `resolve` answers an advert only with the
            // record carrying its pin. A typed address names no identity: if its saved pin is
            // refused, another OS of the same machine may hold the lease, so pair that one by PIN.
            known != null && known.fpHex.isNotEmpty() -> doConnect(
                targetHost, targetPort, known.name, known.fpHex, oneOffPreset, launch,
                onMismatch = if (saved == null && dh == null) {
                    {
                        pendingTrust = PendingTrust(
                            targetHost, targetPort, typed ?: targetHost, null,
                            PendingTrust.Kind.FP_CHANGED, oneOffPreset, launch,
                        )
                    }
                } else {
                    null
                },
            )
            // Host explicitly advertised pair=optional → trust-on-first-use is permitted (offer it,
            // clearly labeled, alongside PIN pairing). Smart-cast: this branch ⇒ dh != null.
            dh?.pairingRequired == false -> pendingTrust = PendingTrust(
                targetHost, targetPort, name, dh.fingerprint, PendingTrust.Kind.TRUST_NEW,
                oneOffPreset, launch,
            )
            // pair=required, a manual/unknown-policy host, or a card saved without a pin → offer the
            // two ways in: a no-PIN "request access" (approve in the console) or the PIN ceremony.
            else -> pendingTrust = PendingTrust(
                targetHost, targetPort, name, adv, PendingTrust.Kind.REQUEST_ACCESS,
                oneOffPreset, launch,
            )
        }
    }

    // A speed test in flight: which host+preset it is measuring, and how far it has got. The
    // measurement is over a real connect, so it takes the same `connecting` gate every dial does.
    var speedTest by remember { mutableStateOf<HostCardEntry?>(null) }
    var speedTestPhase by remember { mutableStateOf<SpeedTestPhase>(SpeedTestPhase.Connecting) }

    fun startSpeedTest(entry: HostCardEntry) {
        val id = requireIdentity() ?: return
        // The magic packet isn't the only thing LNP blocks: without the grant this would EPERM its
        // way to a timeout and report a dead link on a perfectly good one.
        if (!lnpGranted) {
            lnpPrompt = true
            return
        }
        speedTest = entry
        speedTestPhase = SpeedTestPhase.Connecting
        notice = null
        connecting = true
        discovery.removeListener(subscriber) // a browse running through the burst would measure itself
        scope.launch {
            runSpeedTest(context, id, entry.host.address, entry.host.port, entry.host.fpHex) { p ->
                // A dismissed dialog abandons the run; don't drag it back onto the screen.
                if (speedTest != null) speedTestPhase = p
            }
            connecting = false
            discovery.addListener(subscriber)
        }
    }

    // Toggle a host+preset pin. Presentation only: it never touches the preset itself and never
    // changes the host's default binding.
    fun togglePin(kh: KnownHost, preset: StreamPreset) {
        val pins = if (preset.id in kh.pinnedPresetIds) {
            kh.pinnedPresetIds - preset.id
        } else {
            kh.pinnedPresetIds + preset.id
        }
        knownHostStore.save(kh.copy(pinnedPresetIds = pins))
        savedHosts = knownHostStore.all()
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
        if (copied) notice = message else status = message
    }

    // Host actions (`design/host-actions.md` §7) — sleep, restart or shut the host down. The
    // menu rows come from what the HOST said it lets this device do, so a device without the
    // Host-power grant is offered none; a destructive one still asks first, because losing what
    // is running on that machine is not something a mis-tap should be able to do.
    fun runHostAction(kh: KnownHost, a: HostActions.Action) {
        val id = requireIdentity() ?: return
        val name = kh.name.ifBlank { kh.address }
        notice = "${a.label} — asking $name…"
        status = null
        // Whatever the host said about itself is about to be wrong: ask again next sweep.
        hostActionsAt = hostActionsAt - kh.fpHex
        scope.launch {
            notice = withContext(Dispatchers.IO) {
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
                notice = a.unavailableReason.ifEmpty { "${a.label} isn't available right now" }
            a.danger -> confirmAction = kh to a
            else -> runHostAction(kh, a)
        }
    }

    // "Send logs to host" — [SendLogs], the same upload the console's host menu runs. The outcome
    // is a notice either way (success and failure both name the host), because the row's whole job
    // is to tell a reporter whether the bundle actually landed.
    fun sendLogs(kh: KnownHost) {
        val id = requireIdentity() ?: return
        notice = "Sending logs to ${kh.name.ifBlank { kh.address }}…"
        status = null
        scope.launch {
            val message = withContext(Dispatchers.IO) { SendLogs.toHost(context, id, kh) }
            notice = message
        }
    }

    // ---- punktfunk:// routing (design/client-deep-links.md §3) --------------------------------
    //
    // The invariant: a URL may only ever do what a click on an existing card could do, MINUS trust
    // decisions. So it never pairs, never trusts on its own, and carries references rather than
    // values. Everything below is either "do exactly what the card does" or "refuse and say why" —
    // a shortcut that can't honour its reference must say so, because streaming with the wrong
    // settings is worse than an explanatory notice.
    LaunchedEffect(deepLink, identity, savedHosts) {
        val url = deepLink ?: return@LaunchedEffect
        // Wait for the identity rather than refusing: it arrives a beat after first composition and
        // the effect re-runs when it does.
        if (identity == null) return@LaunchedEffect
        onDeepLinkHandled()
        val parsed = DeepLinks.parse(url)
        if (parsed is DeepLinkResult.Refused) {
            // A link for someone else's scheme is not our business to complain about.
            if (parsed.error != LinkError.NOT_OUR_SCHEME) status = parsed.message()
            return@LaunchedEffect
        }
        val link = (parsed as DeepLinkResult.Parsed).link
        if (link.route != LinkRoute.CONNECT) {
            // `wake` and `browse` are reserved in the grammar and parse today; a front-end that
            // hasn't implemented them refuses with a notice rather than silently connecting.
            status = "Punktfunk on Android can't do “${link.route.word}” links yet."
            return@LaunchedEffect
        }
        // A preset reference that can't be honoured refuses: a "Work" shortcut streaming with the
        // wrong settings is worse than an error naming what failed.
        val presetRef = link.preset
        if (presetRef != null) {
            val (_, resolution) = presetStore.resolve(presetRef)
            if (resolution != PresetResolution.FOUND) {
                status = if (resolution == PresetResolution.AMBIGUOUS) {
                    "More than one preset is called “$presetRef” — rename one and try again."
                } else {
                    "That link asks for a preset called “$presetRef”, which isn't on this device."
                }
                return@LaunchedEffect
            }
        }
        when (val resolved = DeepLinks.resolveHost(link, savedHosts)) {
            // A saved record. Pinned AND named by its (unguessable) id is the one-click contract:
            // do exactly what tapping its card does. Named by anything a web page could guess —
            // its label, its address — the same dial waits for a tap on the confirmation.
            is HostResolution.Record -> {
                // A pin that contradicts the stored one is the link being stale or lying. Hard
                // refusal: this is the one case where doing what the card does would be wrong.
                if (link.pinConflict(resolved.host)) {
                    status = "That link's fingerprint doesn't match the one pinned for " +
                        "${resolved.host.name} — it's out of date, or it isn't that host."
                    return@LaunchedEffect
                }
                if (resolved.host.fpHex.isEmpty()) {
                    // Saved but never pinned (nothing writes such a record today, but the rule is
                    // absolute): a link may not establish trust, so this is a confirmation.
                    pendingTrust = PendingTrust(
                        resolved.host.address, resolved.host.port, resolved.host.name,
                        link.fp, PendingTrust.Kind.REQUEST_ACCESS, presetRef, link.launch,
                    )
                    return@LaunchedEffect
                }
                if (resolved is HostResolution.Confirm) {
                    pendingLinkConnect = PendingLinkConnect(resolved.host, presetRef, link.launch)
                    return@LaunchedEffect
                }
                connect(
                    resolved.host.address, resolved.host.port,
                    oneOffPreset = presetRef, launch = link.launch, saved = resolved.host,
                )
            }
            // Unknown, or known only by address: the confirmation sheet, from which the normal
            // pairing flow proceeds under the user's eyes. Never a silent trust.
            is HostResolution.Unknown -> pendingTrust = PendingTrust(
                resolved.address,
                resolved.port,
                link.name ?: resolved.address,
                resolved.fp,
                PendingTrust.Kind.REQUEST_ACCESS,
                presetRef,
                link.launch,
            )
            HostResolution.Ambiguous ->
                status = "More than one saved host is called “${link.hostRef}” — " +
                    "rename one, or use its address."
            HostResolution.Unresolvable ->
                status = "That link points at a host this device doesn't know."
        }
    }

    var showManualSheet by remember { mutableStateOf(false) }

    // Wake a saved host on demand — the touch card's Wake item and the console options dialog run
    // the same action. Through the WakeController, so it shows the "Waking…" overlay and waits for
    // the host to come back rather than firing one silent packet at it.
    fun wakeHost(kh: KnownHost) {
        // The magic packet is UDP broadcast — LNP-blocked like everything else.
        if (!lnpGranted) {
            lnpPrompt = true
            return
        }
        waker.start(
            hostName = kh.name,
            connectsAfter = false,
            macs = kh.mac,
            lastIp = kh.address,
            // "Back up" is the host answering a probe, at the address its advert claims if a cold
            // boot moved it — never the advert alone, which a sleeping host keeps publishing.
            isOnline = {
                val live = discovered.firstOrNull { kh.matches(it) }
                withContext(Dispatchers.IO) {
                    Presence.isSelf(
                        kh,
                        NativeBridge.nativeProbe(
                            live?.host ?: kh.address, live?.port ?: kh.port, 3_000,
                        ),
                    )
                }
            },
            onOnline = {},
        )
    }

    fun forgetHost(kh: KnownHost) {
        knownHostStore.remove(kh)
        // A forgotten host leaves no list of what somebody plays, and no record of what they were
        // playing, behind on the device. Its record id is the key both are filed under, so this is
        // the last moment either can be found.
        io.unom.punktfunk.kit.library.LibraryCache.standard(context.cacheDir).forget(kh.id)
        LibraryPosition.forget(context, kh.id)
        // The resolver already ignores a dangling pointer, so this is hygiene: without it a later
        // re-pair of a different box would inherit somebody's old choice.
        if (settings.defaultHost == kh.id) {
            onSettingsChange(settings.copy(defaultHost = null))
        }
        savedHosts = knownHostStore.all()
    }

    /** Point the start-screen setting at [kh], or clear it. The caller persists. */
    fun setDefaultHost(kh: KnownHost, on: Boolean) {
        onSettingsChange(settings.copy(defaultHost = if (on) kh.id else null))
    }

    ConnectGrid(
        savedHosts = savedHosts,
        discovered = discovered,
        discoveredUnsaved = discoveredUnsaved,
        reachable = reachable,
        presets = presets,
        pinsFor = presetStore::pinsFor,
        connecting = connecting,
        notice = notice,
        status = status,
        lnpGranted = lnpGranted,
        onAskLocalNetwork = { lnpPrompt = true },
        onConnect = { kh, oneOff -> connect(kh.address, kh.port, oneOffPreset = oneOff, saved = kh) },
        onConnectDiscovered = { dh -> connect(dh.host, dh.port, dh) },
        onForget = { kh -> forgetHost(kh) },
        onEdit = { kh -> editTarget = kh },
        onWake = { kh -> wakeHost(kh) },
        onSpeedTest = { kh -> startSpeedTest(HostCardEntry(kh, null)) },
        onSendLogs = { kh -> sendLogs(kh) },
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
            connecting = connecting,
            modeLabel = "$w×$h@$hz",
            onDismiss = { showManualSheet = false },
            onConnect = { h2, p, n -> connect(h2, p, manualName = n) },
        )
    }

    // Which layer a measurement would land in. Resolved here, not in the prompt: it is a question
    // for the preset store, and the Apply button and the caption above it must agree on the answer.
    val speedTestTarget = speedTest?.let { SpeedTestTarget.resolve(it.host, it.pin?.id, presetStore) }
    // Prefill a not-yet-learned MAC from the host's live advert, mirroring Apple's
    // `discovery.hosts.first { host.matches($0) }?.macAddresses`.
    val editSuggestedMacs =
        editTarget?.let { kh -> discovered.firstOrNull { kh.matches(it) }?.mac } ?: emptyList()

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
    // ConnectPrompts.kt. It decides nothing: each action below lands right back in the engine above.
    ConnectPrompts(
        identity = identity,
        presets = presets,
        isOnline = { it.isOnline(reachable) },
        pendingTrust = pendingTrust,
        onPendingTrustChange = { pendingTrust = it },
        onTrustNew = { pt ->
            pendingTrust = null
            doConnect(pt.host, pt.port, pt.name, null, pt.preset, pt.launch)
        },
        onPaired = { pt, fp ->
            knownHostStore.trust(pt.host, pt.port, pt.name, fp, paired = true)
            savedHosts = knownHostStore.all()
            pendingTrust = null
            doConnect(pt.host, pt.port, pt.name, fp, pt.preset, pt.launch)
        },
        onRequestAccess = { pt -> pendingTrust = null; requestAccess(pt) },
        pendingLinkConnect = pendingLinkConnect,
        onConfirmLinkConnect = { plc ->
            pendingLinkConnect = null
            connect(
                plc.host.address, plc.host.port,
                oneOffPreset = plc.preset, launch = plc.launch, saved = plc.host,
            )
        },
        onDismissLinkConnect = { pendingLinkConnect = null },
        awaitingHostName = awaiting?.target?.name,
        onCancelApproval = {
            awaiting?.cancelled?.set(true)
            awaiting = null
            connecting = false
            discovery.addListener(subscriber) // the request may still be pending on the host; keep scanning
        },
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
                notice = "%.0f Mbit/s set in %s".format(done.recommendedMbps, where)
            }
            speedTest = null
        },
        onDismissSpeedTest = { speedTest = null },
        editTarget = editTarget,
        editSuggestedMacs = editSuggestedMacs,
        onSaveHost = { updated ->
            knownHostStore.save(updated)
            savedHosts = knownHostStore.all()
            editTarget = null
        },
        onDismissEdit = { editTarget = null },
        lnpPrompt = lnpPrompt,
        onAllowLocalNetwork = {
            lnpPrompt = false
            onAskLocalNetwork()
        },
        onOpenSystemSettings = {
            lnpPrompt = false
            context.startActivity(
                Intent(
                    android.provider.Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                    Uri.fromParts("package", context.packageName, null),
                ),
            )
        },
        onDismissLnpPrompt = { lnpPrompt = false },
        connectingHostName = attempt?.hostName,
        waker = waker,
        onCancelConnect = { cancelConnect() },
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
