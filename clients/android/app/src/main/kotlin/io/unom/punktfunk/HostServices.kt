package io.unom.punktfunk

import android.content.Context
import android.os.SystemClock
import android.util.Log
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.Presence
import io.unom.punktfunk.kit.discovery.PresenceTracker
import io.unom.punktfunk.kit.discovery.WakeLoop
import io.unom.punktfunk.kit.library.GameOverrides
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.models.ActiveSession
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.isActive
import kotlinx.coroutines.withContext

/**
 * What one dial asks for. [pinHex] is the pinned fingerprint (empty ⇒ TOFU); [launch] a
 * store-qualified library id to boot into, or `null` for the desktop; [dialer] names the shell
 * and path, for the log and the wire. [deliveryFlags] is [DELIVERY_PROBE_ONLY] for a network
 * check; [profile] the host profile to play as, `null` for none. [wakeSince] is when the host
 * first refused as still waking ([HostNative.elapsedMs]), `null` until it has. [promptsUnlock]:
 * the shell that dials shows the password prompt after a `profile-locked` refusal. [overrides]
 * are what the host sets for the [launch]ed game, applied over [settings] ([underHost]).
 */
data class DialSpec(
    val settings: Settings,
    val identity: ClientIdentity,
    val host: String,
    val port: Int,
    val pinHex: String,
    val launch: String?,
    val dialer: String,
    val timeoutMs: Int = CONNECT_TIMEOUT_MS,
    val preset: StreamPreset? = null,
    val deliveryFlags: Int = 0,
    val profile: String? = null,
    val wakeSince: Long? = null,
    val promptsUnlock: Boolean = false,
    val overrides: GameOverrides? = null,
)

/** The calls [HostServices] makes below Kotlin: [JniHostNative] on device, a fake in the JVM tests. */
interface HostNative {
    /**
     * A session handle for [spec], or `0` with the cause in [lastError]. Never cancelled midway.
     * [unlock] rides this one dial and is wiped once the native call returns.
     */
    suspend fun connect(spec: DialSpec, unlock: Unlock?): Long

    /** The token of the last failed connect or pairing, cleared on read. */
    fun lastError(): String

    /** What the host said with the last refusal, cleared on read; empty when it said none. */
    fun lastSaid(): String = ""
    fun close(handle: Long)

    /** The fingerprint the host on [handle] presented; empty when none. */
    fun fingerprint(handle: Long): String

    /** The management port the host's Welcome named; `0` when it named none. */
    fun mgmtPort(handle: Long): Int

    /** The fingerprint that answered at [address]:[port], or `null` when nothing did. Blocking. */
    fun probe(address: String, port: Int, timeoutMs: Int): String?
    fun wakeOnLan(macs: List<String>, lastIp: String)

    /** The PIN ceremony: the host's fingerprint, or `""`. Blocking. */
    fun pair(address: String, port: Int, identity: ClientIdentity, pin: String, deviceName: String): String

    /** The no-PIN approval wait: the host's fingerprint, or `""`. Blocking. */
    fun requestAccess(
        address: String,
        port: Int,
        identity: ClientIdentity,
        pinHex: String,
        deviceName: String,
    ): String

    /** Milliseconds before dialing again a host that refused with [token]; -1 = show it. */
    fun wakeRedialDelayMs(autoWake: Boolean, token: String, sinceFirstMs: Long): Long

    /** The same for the session on [handle], once it ended. */
    fun sessionWakeRedialDelayMs(handle: Long, autoWake: Boolean, sinceFirstMs: Long): Long

    /** The wake loop's clock, in milliseconds. */
    fun elapsedMs(): Long
    fun sleep(ms: Long)
}

/** [HostNative] over the JNI bridge. [context] resolves the dial's mode, HDR and device name. */
class JniHostNative(private val context: Context) : HostNative {
    override suspend fun connect(spec: DialSpec, unlock: Unlock?): Long = connectToHost(context, spec, unlock)
    override fun lastError(): String = NativeBridge.nativeTakeLastError()
    override fun lastSaid(): String = NativeBridge.nativeTakeLastSaid()
    override fun close(handle: Long) = NativeBridge.nativeClose(handle)
    override fun fingerprint(handle: Long): String = NativeBridge.nativeHostFingerprint(handle)
    override fun mgmtPort(handle: Long): Int = NativeBridge.nativeHostMgmtPort(handle)
    override fun probe(address: String, port: Int, timeoutMs: Int): String? =
        NativeBridge.nativeProbe(address, port, timeoutMs)

    override fun wakeOnLan(macs: List<String>, lastIp: String) {
        NativeBridge.nativeWakeOnLan(macs.joinToString(","), lastIp)
    }

    override fun pair(address: String, port: Int, identity: ClientIdentity, pin: String, deviceName: String): String =
        NativeBridge.nativePair(address, port, identity.certPem, identity.privateKeyPem, pin, deviceName)

    override fun requestAccess(
        address: String,
        port: Int,
        identity: ClientIdentity,
        pinHex: String,
        deviceName: String,
    ): String = NativeBridge.nativeRequestAccess(
        address, port, identity.certPem, identity.privateKeyPem, pinHex, deviceName,
    )

    override fun wakeRedialDelayMs(autoWake: Boolean, token: String, sinceFirstMs: Long): Long =
        NativeBridge.nativeWakeRedialDelayMs(autoWake, token, sinceFirstMs)

    override fun sessionWakeRedialDelayMs(handle: Long, autoWake: Boolean, sinceFirstMs: Long): Long =
        NativeBridge.nativeSessionWakeRedialDelayMs(handle, autoWake, sinceFirstMs)

    override fun elapsedMs(): Long = SystemClock.elapsedRealtime()
    override fun sleep(ms: Long) = Thread.sleep(ms)
}

/**
 * Dial, pin, wake and presence for both shells: the touch home, the touch shelf, the speed test
 * and the console all reach a host through one of these. [store] is where trust lands; the host
 * facts the console's rows show are kept here too.
 *
 * Plain Kotlin and thread-safe: a shell calls the suspending dial from its own main-thread
 * scope, so the cancel check and the session hand-over run on that thread, and the blocking
 * calls from its IO threads.
 */
class HostServices(val store: KnownHostStore, private val native: HostNative) {
    /** A dial's Cancel. A dial cancelled while out closes the session it brings back unseen. */
    class Ticket {
        private val flag = AtomicBoolean(false)
        val cancelled: Boolean get() = flag.get()
        fun cancel() = flag.set(true)
    }

    /** The newest of one kind of request: [next] makes every earlier claim stale. */
    class Latest {
        private val gen = AtomicLong(0)
        fun next(): Long = gen.incrementAndGet()
        fun isCurrent(claim: Long): Boolean = gen.get() == claim
    }

    /** What [open] hands back. */
    sealed interface Opened

    /** What [dial] hands back. */
    sealed interface Dialed

    data class Open(val handle: Long) : Opened

    /** [record] is the host's record after the dial; [pinned] when the dial saved it. */
    data class Connected(val session: ActiveSession, val record: KnownHost?, val pinned: Boolean) : Dialed

    /** No session: [token] is the native cause (`""` when none was given). */
    data class Failed(val token: String, val said: String = "") : Opened, Dialed {
        /** Nothing typed came back: the host may be asleep or off the path. */
        val unreachable: Boolean get() = token == "timeout" || token == "io" || token.isEmpty()
    }

    data object Cancelled : Opened, Dialed

    /** The host's fingerprint after a pairing, or the failure token. */
    sealed interface Paired {
        data class Ok(val fp: String) : Paired
        data class Failed(val token: String) : Paired
    }

    /** What one presence sweep found: who answered, and whether that or a record moved. */
    data class Swept(val reachable: Set<String>, val changed: Boolean, val moved: Boolean)

    // ---- dial ---------------------------------------------------------------------------------

    /**
     * Opens a session as [spec] asks and hands back its handle, with no record kept: the speed
     * test's dial. A [ticket] cancelled while the dial was out closes the late session. [unlock]
     * goes with this dial only.
     */
    suspend fun open(spec: DialSpec, ticket: Ticket? = null, unlock: Unlock? = null): Opened {
        val handle = connect(spec, unlock)
        if (ticket?.cancelled == true) {
            if (handle != 0L) withContext(Dispatchers.IO) { native.close(handle) }
            return Cancelled
        }
        return if (handle != 0L) Open(handle) else Failed(native.lastError(), native.lastSaid())
    }

    /**
     * Dials to stream. [record] is the host's saved record, or `null` for a host not saved yet;
     * [name] labels the record a first dial saves. A host with no record, or one a request-access
     * approval just let in ([requestAccess]), is saved with the identity it presented, paired
     * when it was approved or already paired, so the next dial is silent. A woken host that
     * refuses before it can stream is dialed again ([wakeRedial]) under the same overlay.
     * [unlock] rides the first dial only; a redial sends none.
     */
    suspend fun dial(
        spec: DialSpec,
        record: KnownHost?,
        name: String,
        requestAccess: Boolean = false,
        ticket: Ticket? = null,
        unlock: Unlock? = null,
    ): Dialed {
        var dialing = spec
        var opened = open(dialing, ticket, unlock)
        while (opened is Failed && !requestAccess) {
            val token = opened.token
            val (next, wait) = wakeRedial(dialing) { autoWake, since ->
                native.wakeRedialDelayMs(autoWake, token, since)
            } ?: break
            Log.i(TAG, "host not ready to stream — dialing again in $wait ms")
            delay(wait)
            if (ticket?.cancelled == true) return Cancelled
            dialing = next
            opened = open(dialing, ticket)
        }
        val handle = when (opened) {
            is Open -> opened.handle
            is Failed -> return opened
            Cancelled -> return Cancelled
        }
        var dialed = record
        var pinned = false
        if (record == null || (requestAccess && !record.paired)) {
            native.fingerprint(handle).takeIf { it.isNotEmpty() }?.let { fp ->
                val paired = requestAccess || record?.paired == true
                dialed = store.trust(spec.host, spec.port, record?.name ?: name, fp, paired)
                pinned = true
            }
        }
        // A redial is pinned to the identity this session proved.
        val redial = dialing.copy(pinHex = spec.pinHex.ifEmpty { native.fingerprint(handle) })
        val session = SessionFactory.afterDial(
            handle, dialed, spec.settings.underHost(spec.overrides), spec.preset, store, native.mgmtPort(handle), redial,
        )
        return Connected(session, dialed, pinned)
    }

    /**
     * One launch, one session: every shell's dial lands here, so the refusal lives here too. A
     * refused dial returns `0` without touching the host. A coroutine cancelled while the dial
     * was out has nobody to hand the session to, so it closes here. [unlock] is wiped either way.
     */
    private suspend fun connect(spec: DialSpec, unlock: Unlock?): Long {
        if (!SessionGate.take()) {
            unlock?.wipe()
            Log.w(TAG, "dial refused (${spec.dialer}): this device already has a session or a dial in hand")
            return 0L
        }
        try {
            val handle = native.connect(spec, unlock)
            if (handle != 0L && !currentCoroutineContext().isActive) {
                withContext(Dispatchers.IO + NonCancellable) { native.close(handle) }
            }
            currentCoroutineContext().ensureActive()
            return handle
        } finally {
            SessionGate.release()
        }
    }

    /**
     * The spec and wait for dialing [spec]'s host again after a refusal, or `null` to show it.
     * [delayMs] is the core's woken-host rule for that refusal, given auto-wake and the time
     * since the first one (-1: none yet).
     */
    private fun wakeRedial(spec: DialSpec, delayMs: (Boolean, Long) -> Long): Pair<DialSpec, Long>? {
        val now = native.elapsedMs()
        val wait = delayMs(spec.settings.autoWakeEnabled, spec.wakeSince?.let { now - it } ?: -1)
        return if (wait < 0) null else spec.copy(wakeSince = spec.wakeSince ?: now) to wait
    }

    /** [wakeRedial] for a [session] that ended, asked before it closes. */
    fun wakeRedial(session: ActiveSession): Pair<DialSpec, Long>? {
        val spec = session.redial ?: return null
        return wakeRedial(spec) { autoWake, since -> native.sessionWakeRedialDelayMs(session.handle, autoWake, since) }
    }

    // ---- pin ----------------------------------------------------------------------------------

    /** The SPAKE2 PIN ceremony with the host at [address]:[port]. Blocking. */
    fun pair(address: String, port: Int, identity: ClientIdentity, pin: String, deviceName: String): Paired =
        paired { native.pair(address, port, identity, pin, deviceName) }

    /**
     * Asks for access with no PIN and waits for the host's operator, pinned to [pinHex] (empty ⇒
     * trust on first use). Never streams. Blocking, up to ~185 s.
     */
    fun requestAccess(
        address: String,
        port: Int,
        identity: ClientIdentity,
        pinHex: String,
        deviceName: String,
    ): Paired = paired { native.requestAccess(address, port, identity, pinHex, deviceName) }

    /** Save the host a pairing answered as, paired, under [name]. */
    fun trustPaired(address: String, port: Int, name: String, fp: String): KnownHost =
        store.trust(address, port, name, fp, paired = true)

    /** The latest pairing: an older one's answer is stale once another starts. */
    val pairings = Latest()

    private fun paired(ask: () -> String): Paired {
        val fp = runCatching(ask).getOrDefault("")
        return if (fp.isNotEmpty()) Paired.Ok(fp) else Paired.Failed(native.lastError())
    }

    // ---- wake ---------------------------------------------------------------------------------

    /** One wake at a time: a newer claim, or a bare [Latest.next], stops the older wait. */
    val wakes = Latest()

    /** One magic packet to [macs], now. A host that is up ignores it. */
    fun sendWake(macs: List<String>, lastIp: String) = native.wakeOnLan(macs, lastIp)

    /**
     * Whether the host pinned to [pinHex] answers as itself: at [live]'s address when there is an
     * advert (a cold boot can move the lease), else at [address]:[port]. Blocking.
     */
    fun answers(pinHex: String, address: String, port: Int, live: DiscoveredHost?, timeoutMs: Int): Boolean =
        Presence.isSelf(pinHex, native.probe(live?.host ?: address, live?.port ?: port, timeoutMs))

    /** [answers] for a saved record, which carries its own pin and address. */
    fun answers(kh: KnownHost, live: DiscoveredHost?, timeoutMs: Int): Boolean =
        answers(kh.fpHex, kh.address, kh.port, live, timeoutMs)

    /**
     * Wakes [macs] at [lastIp] and waits for [isOnline] ([WakeLoop]) while [claim] is the latest
     * of [wakes] and nothing [cancelled] it. True when the host came up. Blocking.
     */
    fun wake(
        claim: Long,
        macs: List<String>,
        lastIp: String,
        isOnline: () -> Boolean,
        cancelled: () -> Boolean = { false },
        onTick: (seconds: Int, timedOut: Boolean, online: Boolean) -> Unit,
    ): Boolean = WakeLoop.run(
        send = { native.wakeOnLan(macs, lastIp) },
        isOnline = isOnline,
        cancelled = { !wakes.isCurrent(claim) || cancelled() },
        onTick = onTick,
        now = native::elapsedMs,
        sleep = native::sleep,
    )

    // ---- presence -----------------------------------------------------------------------------

    /** The live adverts, as the shell's browse last delivered them. */
    @Volatile var discovered: List<DiscoveredHost> = emptyList()

    /** Record ids that answered the last sweeps ([PresenceTracker]'s grace included). */
    @Volatile var reachable: Set<String> = emptySet()
        private set

    private val tracker = PresenceTracker()

    /**
     * Probes every host in [saved] ([Presence.sweep]) at its address and at [liveFor]'s advert,
     * follows a pinned host that answered somewhere else, and folds the answers into
     * [reachable]. Blocking.
     */
    fun sweep(saved: List<KnownHost>, liveFor: (KnownHost) -> DiscoveredHost?): Swept {
        val up = Presence.sweep(saved, liveFor) { addr, port -> native.probe(addr, port, Presence.PROBE_MS) }
        val moved = saved.any { kh -> up[kh.id]?.let { store.learnAddress(kh.fpHex, it.address, it.port) } == true }
        synchronized(tracker) {
            val next = tracker.apply(saved.map { it.id }.toSet(), up.keys)
            val changed = next != reachable
            reachable = next
            return Swept(next, changed, moved)
        }
    }

    // ---- host facts ---------------------------------------------------------------------------

    /** What each paired host last said this device may do TO it, by fingerprint. */
    val hostActions = ConcurrentHashMap<String, List<HostActions.Action>>()

    /** The title each paired host has up, by fingerprint; `""` for none. */
    val nowPlaying = ConcurrentHashMap<String, String>()

    /** The screens each paired host offers this device and its pick, by fingerprint. Absent = no Screen section. */
    val screens = ConcurrentHashMap<String, DisplayTargets.Targets>()

    /** When [hostActions], [nowPlaying] and [screens] were last asked, by fingerprint: their TTLs run from here. */
    val hostActionsAt = ConcurrentHashMap<String, Long>()
    val nowPlayingAt = ConcurrentHashMap<String, Long>()
    val screensAt = ConcurrentHashMap<String, Long>()

    private companion object {
        const val TAG = "punktfunk"
    }
}
