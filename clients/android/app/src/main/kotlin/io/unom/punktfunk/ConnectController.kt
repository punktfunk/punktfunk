package io.unom.punktfunk

import android.content.Context
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.library.GameOverrides
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.HostResolution
import io.unom.punktfunk.kit.link.LinkError
import io.unom.punktfunk.kit.link.LinkRoute
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityHolder
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.models.PendingLinkConnect
import io.unom.punktfunk.models.PendingTrust
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * A no-PIN "request access" connect in flight: the host asked (it labels the cancelable "Waiting
 * for approval…" dialog) and the flag its Cancel trips. The connect blocks with no abort, so a
 * late result checks [cancelled] and closes the session, approved or not, instead of navigating.
 */
private class RequestAccessState(val target: PendingTrust) {
    val cancelled = AtomicBoolean(false)
}

/**
 * A plain dial in flight: [hostName] labels the [ConnectOverlay]'s "Connecting…" phase, and its
 * Cancel trips [ticket]. A late handle is closed silently, as [RequestAccessState]'s is.
 */
internal class ConnectAttempt(val hostName: String) {
    val ticket = HostServices.Ticket()

    /** The host parked this dial for its operator: the approval wait replaces "Connecting…". */
    var awaitingApproval by mutableStateOf(false)
        private set

    /** The profile whose desktop this dial's unlock is starting: a progress line, not an approval. */
    var starting by mutableStateOf<String?>(null)
        private set

    /** The profile named by the unlock the dial now out carries; `null` for a bare dial. */
    var unlocking: String? = null
        set(value) {
            field = value
            starting = null
        }

    /** The host said `Pending` about the dial now out. */
    fun hostPending() {
        val profile = unlocking
        if (profile != null) starting = profile else awaitingApproval = true
    }
}

/**
 * The connect screen's engine: the dial and its wake fallback, trust and PIN pairing, request
 * access, the profile ask, Wake and the `punktfunk://` router, with the Compose state they share.
 *
 * [ConnectScreen] remembers one for its whole life, so a recomposition never drops a dial in
 * flight. The [State]s it is handed read the composable's latest settings, local-network grant
 * and connected callback. Every field is written on the main thread.
 */
internal class ConnectController(
    private val context: Context,
    private val scope: CoroutineScope,
    settingsNow: State<Settings>,
    lnpNow: State<Boolean>,
    onConnectedNow: State<(ActiveSession) -> Unit>,
) {
    private val settings by settingsNow
    private val lnpGranted by lnpNow
    private val onConnected by onConnectedNow

    val knownHostStore = KnownHostStore(context)

    /** Dial, pin, wake and presence, over [knownHostStore]. */
    val services = HostServices(knownHostStore, JniHostNative(context))

    /** The settings-preset catalog: what a tap connects with, the one-offs, the pinned cards. */
    val presetStore = PresetStore(context)

    /**
     * mDNS discovery over the native browse; its listener fires on the main thread. App asks the
     * grants it needs; [lnpGranted] only gates what would otherwise EPERM its way to a timeout.
     */
    val discovery: HostDiscovery = HostDiscovery.shared(context)

    /**
     * Wakes a sleeping saved host and waits for it to answer before dialing; its overlay rides
     * over both homes. A cold boot can take a minute and more to answer again.
     */
    val waker = WakeController(scope, services)
    private val identities = IdentityHolder.shared(context)

    var discovered by mutableStateOf<List<DiscoveredHost>>(emptyList())
        private set

    // One value, because subscribing IS what runs the browse: a pause drops this exact subscriber
    // and the resume hands back the same one.
    private val subscriber: (List<DiscoveredHost>) -> Unit = { discovered = it }

    var savedHosts by mutableStateOf(knownHostStore.all())
        private set

    /** Saved hosts the last probe sweep reached, by record id: all that [isOnline] reads. */
    var reachable by mutableStateOf<Set<String>>(emptySet())

    /** The process-wide identity load, mirrored so effects keyed on it rerun when it lands. */
    var identity by mutableStateOf(identities.current)
        private set
    var connecting by mutableStateOf(false)

    /** A failure line; the grid draws it red. */
    var status by mutableStateOf<String?>(null)

    /** A confirmation ("75 Mbit/s set in “Travel”"), never dressed as a [status] failure. */
    var notice by mutableStateOf<String?>(null)

    /** The local-network rationale: raised by the banner, and by a dial or wake without the grant. */
    var lnpPrompt by mutableStateOf(false)

    /** A trust decision awaiting the user: TOFU, a changed pin, PIN pairing or request access. */
    var pendingTrust by mutableStateOf<PendingTrust?>(null)

    /** A link that named a saved host by a guessable reference, awaiting the OK. */
    var pendingLinkConnect by mutableStateOf<PendingLinkConnect?>(null)

    /** The profile picker a dial waits on. */
    var profileAsk by mutableStateOf<ProfileAsk?>(null)
        private set

    /** The password prompt a dial waits on. */
    var unlockAsk by mutableStateOf<PasswordAsk?>(null)
        private set
    var seatWait by mutableStateOf<ProfileWait?>(null)

    // A link's `as=`, taken by the next dial.
    private var linkAs: String? = null
    private var attempt by mutableStateOf<ConnectAttempt?>(null)
    private var awaiting by mutableStateOf<RequestAccessState?>(null)

    /** The host a plain dial is connecting to, for the overlay's "Connecting…" phase. */
    val connectingHostName: String? get() = attempt?.takeUnless { it.awaitingApproval }?.hostName

    /** The profile whose desktop the dial's unlock is starting, for the overlay's progress line. */
    val startingProfile: String? get() = attempt?.starting

    /** The host a request-access connect, or a parked plain dial, waits on: "Waiting for approval…". */
    val awaitingHostName: String?
        get() = awaiting?.target?.name ?: attempt?.takeIf { it.awaitingApproval }?.hostName

    fun refreshHosts() {
        savedHosts = knownHostStore.all()
    }

    /** Hands the browse its subscriber back. */
    fun resumeBrowse() = discovery.addListener(subscriber)

    /** Lets the browse go: a dial, a wake or a speed test wants the radio. */
    fun pauseBrowse() = discovery.removeListener(subscriber)

    /** Loads the identity off the main thread; an Unrecoverable store refuses rather than mints. */
    fun loadIdentity() {
        scope.launch {
            identity = withContext(Dispatchers.IO) { identities.await() }
            if (identity == null) status = context.getString(R.string.android_identity_broken)
        }
    }

    /**
     * Every identity-gated action funnels here: the identity when ready, also when another
     * screen's retry loaded it; else the holder's line, and a failed load retries on this tap.
     */
    fun requireIdentity(): ClientIdentity? {
        (identity ?: identities.current)?.let { identity = it; return it }
        status = identities.blockedMessage()
        if (identities.failed) loadIdentity()
        return null
    }

    // The dial itself, identity ready, through the profile's unlock. A TOFU dial saves what the
    // host presented, unpaired. [overrides] are the host's for the [launch]ed title. [onFailure]
    // takes an unreachable dial (the wake wait), [onMismatch] a refused pin; [redial] marks the
    // one dial a [ProfileRetry] grants.
    private fun doConnectDirect(
        targetHost: String,
        targetPort: Int,
        name: String,
        pinHex: String?,
        preset: StreamPreset?,
        launch: String? = null,
        overrides: GameOverrides? = null,
        onFailure: (() -> Unit)? = null,
        onMismatch: (() -> Unit)? = null,
        redial: Boolean = false,
    ) {
        val id = requireIdentity() ?: return
        val thisAttempt = ConnectAttempt(name)
        attempt = thisAttempt // shows the ConnectOverlay's "Connecting…" phase immediately
        connecting = true
        status = null
        notice = null
        pauseBrowse()
        scope.launch {
            val record = pinHex?.let { knownHostStore.resolve(it, targetHost, targetPort) }
            val choice = chooseProfile(
                knownHostStore, id, record, linkAs.also { linkAs = null },
                wait = { w ->
                    if (w != null && thisAttempt.ticket.cancelled) {
                        w.cancelled.complete(Unit)
                    } else {
                        if (w != null) attempt = null // the wait takes the overlay's place
                        seatWait = w
                    }
                },
            ) { ask ->
                if (thisAttempt.ticket.cancelled) {
                    ask.answer.complete(null)
                } else {
                    attempt = null // the picker takes the overlay's place
                    profileAsk = ask
                }
            }
            profileAsk = null
            if (thisAttempt.ticket.cancelled) return@launch
            if (choice !is ProfileChoice.Dial) {
                connecting = false
                if (choice is ProfileChoice.Refused) status = choice.line.words(context.resources)
                resumeBrowse()
                return@launch
            }
            attempt = thisAttempt
            refreshHosts()
            val spec = DialSpec(
                settings.effectiveFor(preset), id, targetHost, targetPort, pinHex ?: "", launch,
                dialer = "touch/host-grid", preset = preset, profile = choice.id,
                promptsUnlock = promptsUnlock(context), overrides = overrides,
            )
            // While the password prompt or the biometric sheet is up, it takes the overlay's place.
            val hooks = record?.let { r ->
                touchUnlockHooks(context, id, r, ask = { askUnlock(thisAttempt, it) }) { up ->
                    attempt = if (up) null else thisAttempt
                }
            }
            // By this dial's pin (the address may also name the other OS of a dual-boot box); with
            // no saved record, a TOFU dial pins what the host presented, unpaired.
            // A parked dial is the approval wait, or with an unlock the desktop starting.
            val watch = launch {
                while (!thisAttempt.awaitingApproval) {
                    delay(PENDING_POLL_MS)
                    if (thisAttempt.starting == null && NativeBridge.nativeDialPending()) thisAttempt.hostPending()
                }
            }
            val dialed = dialUnlocked(hooks, spec.pinHex, choice.row) { unlock ->
                thisAttempt.unlocking = unlock?.let { choice.row?.displayName?.ifEmpty { null } ?: name }
                services.dial(spec, record, name, ticket = thisAttempt.ticket, unlock = unlock)
            }
            watch.cancel()
            // Cancelled mid-dial or at the password prompt: the UI and the browse are back
            // already, and a just-opened session closed silently.
            if (dialed is HostServices.Cancelled) return@launch
            attempt = null
            connecting = false
            if (dialed is HostServices.Connected) {
                onConnected(dialed.session)
            } else if (dialed is HostServices.Failed) {
                resumeBrowse()
                val token = dialed.token
                if (onFailure != null && dialed.unreachable) {
                    // Clearing `attempt` above and starting the wake here land in one recompose,
                    // so the overlay slides Connecting → Waking without a blank frame.
                    onFailure()
                } else if (onMismatch != null && token == "crypto") {
                    // The saved pin was refused: another identity answers at this address.
                    onMismatch()
                } else if (
                    ProfileRetry.afterRefusal(token, record, choice, redial, knownHostStore) { h, p ->
                        stillListed(id, h, p)
                    }
                ) {
                    linkAs = choice.id
                    doConnectDirect(
                        targetHost, targetPort, name, pinHex, preset, launch, overrides, onFailure, onMismatch,
                        redial = true,
                    )
                } else {
                    // A typed host rejection (busy / versions differ / pairing required) means the
                    // host is awake — waking it would be nonsense; show the stated reason instead.
                    status = dialed.said.ifEmpty {
                        context.getString(ConnectErrors.connectMessage(token, requestAccess = false))
                    }
                    if (token == "profile-unknown") refreshHosts()
                }
            }
        }
    }

    /**
     * Shows the password prompt for [at] in place of its overlay. Cancel there ends the dial the
     * way the overlay's Cancel does, so the dial returns [HostServices.Cancelled] to a UI already
     * handed back.
     */
    private suspend fun askUnlock(at: ConnectAttempt, ask: PasswordAsk): PasswordAnswer? {
        if (at.ticket.cancelled) return null
        attempt = null
        unlockAsk = ask
        val answer = try {
            ask.answer.await()
        } finally {
            unlockAsk = null
        }
        if (answer != null) {
            attempt = at
        } else {
            at.ticket.cancel()
            connecting = false
            resumeBrowse()
        }
        return answer
    }

    /**
     * Cancels a plain dial in flight (the overlay's "Connecting…" phase). The native connect
     * can't be aborted, so this cancels the attempt's ticket, whose late handle the dial closes,
     * and returns the UI now with the browse resumed.
     */
    fun cancelConnect() {
        attempt?.ticket?.cancel()
        attempt = null
        connecting = false
        resumeBrowse()
    }

    /**
     * Wake-aware connect. With auto-wake on and a saved host with a MAC that the probe did not
     * reach, it sends a wake packet and dials at once: a routed host (Tailscale, VPN, another
     * subnet) answers a dial it never advertised for, so presence never gates the dial. Only a
     * failed dial falls into the wake-and-wait flow, which redials once the host answers.
     * Otherwise it dials straight through.
     */
    private fun doConnect(
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
        // and overrides, and the "applies from the next session" footers stay truthful. A title's
        // overrides come from the cached catalog; a cold link has none.
        val preset = presetStore.resolveFor(kh, oneOffPreset, launch)
        val overrides = launch?.let { id ->
            LibraryCache.standard(context.cacheDir).load(LibraryCache.keyFor(kh, pinHex ?: ""))
                ?.games?.firstOrNull { it.id == id }?.overrides
        }
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
            scope.launch(Dispatchers.IO) { services.sendWake(macs, targetHost) }
            doConnectDirect(targetHost, targetPort, name, pinHex, preset, launch, overrides, onMismatch = onMismatch, onFailure = {
                waker.start(
                    hostName = name,
                    connectsAfter = true,
                    macs = macs,
                    lastIp = targetHost,
                    // A live advert would answer in milliseconds and lie (see [isOnline]); this
                    // asks the host itself, at the address it advertises if it moved lease.
                    isOnline = { services.answers(pinHex ?: "", targetHost, targetPort, liveAdvert(), 3_000) },
                    onOnline = {
                        val live = liveAdvert()
                        // Woke back on a new address? Re-point the saved record at it, keeping
                        // the one it left, then dial there (no fallback on this redial — a
                        // second failure surfaces as the plain error).
                        if (live != null && kh != null && knownHostStore.learnAddress(kh.fpHex, live.host, live.port)) {
                            refreshHosts()
                        }
                        doConnectDirect(
                            live?.host ?: targetHost, live?.port ?: targetPort, name, pinHex,
                            preset, launch, overrides, onMismatch = onMismatch,
                        )
                    },
                )
            })
        } else {
            doConnectDirect(targetHost, targetPort, name, pinHex, preset, launch, overrides, onMismatch = onMismatch)
        }
    }

    /**
     * The no-PIN "request access" path: the host parks the request until the operator approves
     * it in its console or web UI. Approval saves the host as paired, like a PIN ceremony, and
     * streams only for a link. Cancel returns the UI at once; a late answer is dropped.
     */
    fun requestAccess(target: PendingTrust) {
        // A request names the fingerprint the host must show. A typed host has none: PIN.
        val pinHex = target.advertisedFp?.takeIf { it.isNotBlank() } ?: run {
            pendingTrust = target.copy(kind = PendingTrust.Kind.PAIR)
            return
        }
        pendingTrust = null
        val id = requireIdentity() ?: return
        val req = RequestAccessState(target)
        awaiting = req
        connecting = true
        status = null
        scope.launch {
            // The advertised fingerprint is the pin: an impostor cannot answer while we wait.
            val paired = withContext(Dispatchers.IO) {
                services.requestAccess(target.host, target.port, id, pinHex, deviceName(context))
            }
            // Cancelled while we waited: don't touch UI a fresh action may now own.
            if (req.cancelled.get()) return@launch
            awaiting = null
            connecting = false
            when (paired) {
                is HostServices.Paired.Ok -> {
                    services.trustPaired(target.host, target.port, target.name, paired.fp)
                    refreshHosts()
                    if (target.fromLink) {
                        doConnect(target.host, target.port, target.name, paired.fp, target.preset, target.launch)
                    } else {
                        notice = context.getString(R.string.pair_paired, target.name)
                    }
                }
                // Cause-specific: an operator denial, an approval timeout, and a request that
                // never reached the host are different problems with different fixes.
                is HostServices.Paired.Failed ->
                    status = context.getString(ConnectErrors.connectMessage(paired.token, requestAccess = true))
            }
        }
    }

    /** Cancel on "Waiting for approval…": the UI returns at once and the answer is dropped. */
    fun cancelApproval() {
        if (awaiting == null) return cancelConnect()
        awaiting?.cancelled?.set(true)
        awaiting = null
        connecting = false
    }

    /**
     * Decides pinned reconnect, TOFU or pairing, then dials. The record is the tapped card's
     * ([saved]), else the one the advertised pin names, else what a typed address answers with:
     * never a record pinned to another fingerprint, since both OS installs of a dual-boot box
     * answer at one lease. TOFU only when the host advertised pair=optional; otherwise request
     * access or the PIN ceremony. [oneOffPreset] is a "Connect with ▸" pick: null follows the
     * host's binding, `""` forces the global defaults, and neither rebinds. [launch] is a
     * library id the host boots straight into.
     */
    fun connect(
        targetHost: String,
        targetPort: Int,
        dh: DiscoveredHost? = null,
        manualName: String? = null,
        oneOffPreset: String? = null,
        launch: String? = null,
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

    /** Trust on first use accepted: dial pinned to the fingerprint the prompt showed, if any. */
    fun trustNew(pt: PendingTrust) {
        pendingTrust = null
        doConnect(
            pt.host, pt.port, pt.name, pt.advertisedFp, pt.preset, pt.launch,
            onMismatch = {
                status = context.getString(R.string.pair_identity_mismatch)
            },
        )
    }

    /** The PIN ceremony finished with [fp]: save the host as paired. Only a link's pairing streams. */
    fun paired(pt: PendingTrust, fp: String) {
        services.trustPaired(pt.host, pt.port, pt.name, fp)
        refreshHosts()
        pendingTrust = null
        if (pt.fromLink) {
            doConnect(pt.host, pt.port, pt.name, fp, pt.preset, pt.launch)
        } else {
            notice = context.getString(R.string.pair_paired, pt.name)
        }
    }

    /** The OK on a link that named a saved host by a guessable reference: the card's own dial. */
    fun confirmLinkConnect(plc: PendingLinkConnect) {
        pendingLinkConnect = null
        linkAs = plc.asProfile
        connect(
            plc.host.address, plc.host.port,
            oneOffPreset = plc.preset, launch = plc.launch, saved = plc.host,
        )
    }

    /**
     * Routes a `punktfunk://` URL (design/client-deep-links.md §3). A link may only do what a tap
     * on a card could, minus trust decisions: it never pairs or trusts on its own, and carries
     * references rather than values. A reference it can't honour refuses with a line saying why,
     * because streaming with the wrong settings is worse than a notice.
     */
    fun openLink(url: String) {
        val parsed = DeepLinks.parse(url)
        if (parsed is DeepLinkResult.Refused) {
            // A link for someone else's scheme is not our business to complain about.
            if (parsed.error != LinkError.NOT_OUR_SCHEME) status = parsed.message().words(context.resources)
            return
        }
        val link = (parsed as DeepLinkResult.Parsed).link
        if (link.route != LinkRoute.CONNECT) {
            // `wake` and `browse` are reserved in the grammar and parse today; a front-end that
            // hasn't implemented them refuses with a notice rather than silently connecting.
            status = context.getString(R.string.link_route_unsupported, link.route.word)
            return
        }
        val presetRef = link.preset
        if (presetRef != null) {
            val (_, resolution) = presetStore.resolve(presetRef)
            if (resolution != PresetResolution.FOUND) {
                status = if (resolution == PresetResolution.AMBIGUOUS) {
                    context.getString(R.string.link_ambiguous_preset, presetRef)
                } else {
                    context.getString(R.string.link_unknown_preset, presetRef)
                }
                return
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
                    status = context.getString(R.string.link_pin_conflict, resolved.host.name)
                    return
                }
                if (resolved.host.fpHex.isEmpty()) {
                    // Saved but never pinned: a link may not establish trust, so this asks first.
                    pendingTrust = PendingTrust(
                        resolved.host.address, resolved.host.port, resolved.host.name,
                        link.fp, PendingTrust.Kind.REQUEST_ACCESS, presetRef, link.launch,
                        fromLink = true,
                    )
                    return
                }
                if (resolved is HostResolution.Confirm) {
                    pendingLinkConnect = PendingLinkConnect(resolved.host, presetRef, link.launch, link.asProfile)
                    return
                }
                linkAs = link.asProfile
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
                fromLink = true,
            )
            HostResolution.Ambiguous ->
                status = context.getString(R.string.link_ambiguous_host, link.hostRef)
            HostResolution.Unresolvable ->
                status = context.getString(R.string.link_host_not_saved)
        }
    }

    /**
     * The card's Wake: through [waker], so it shows the "Waking…" overlay and waits for the host
     * to answer rather than firing one silent packet at it.
     */
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
            isOnline = { services.answers(kh, discovered.firstOrNull { kh.matches(it) }, 3_000) },
            onOnline = {},
        )
    }
}
