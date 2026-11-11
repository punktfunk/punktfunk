package io.unom.punktfunk.kit.discovery

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.util.Log
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.nameOrder

private const val TAG = "PunktfunkMdns"

/** One resolved host fit for the picker. [key] is the stable dedup id. */
data class DiscoveredHost(
    val key: String,
    val name: String,
    val host: String,
    val port: Int,
    val fingerprint: String? = null, // TXT "fp" (host cert SHA-256, advisory — TOFU still verifies)
    val pairingRequired: Boolean = false,
    val mac: List<String> = emptyList(), // TXT "mac" (wake-capable NIC MAC(s), for Wake-on-LAN)
    val os: String = "", // TXT "os" (OS-identity chain, e.g. "linux/fedora/bazzite"); "" on older hosts
    // TXT "mgmt" — the management-API port the library is served on, distinct from `port` (the
    // native QUIC plane). null on an older host / older native lib, meaning "assume 47990".
    val mgmtPort: Int? = null,
)

/** Field separator the native browse uses inside one record (ASCII Unit Separator). */
private const val FIELD_SEP = '\u001F'

/**
 * Parse one record from [NativeBridge.nativeDiscoveryPoll] (`key␟name␟addr␟port␟fp␟pair␟mac␟os␟mgmt`),
 * or null if it's malformed. Fields past the 6th are optional — an older native lib omits them
 * (`mac` 7th, `os` 8th). Pure — unit-tested without Android (see ParseRecordTest). The native side
 * already applied the protocol gate and address selection, so this is just field marshaling.
 */
fun parseHostRecord(record: String): DiscoveredHost? {
    val f = record.split(FIELD_SEP)
    if (f.size < 6) return null
    val addr = f[2]
    val port = f[3].toIntOrNull() ?: return null
    if (addr.isBlank() || port !in 1..65535) return null
    return DiscoveredHost(
        key = f[0].ifBlank { "$addr:$port" },
        name = f[1].ifBlank { addr },
        host = addr,
        port = port,
        fingerprint = f[4].ifBlank { null },
        pairingRequired = f[5] == "required",
        mac = if (f.size > 6) f[6].split(",").map { it.trim() }.filter { it.isNotEmpty() }
        else emptyList(),
        os = if (f.size > 7) sanitizeOsChain(f[7]) else "",
        // 9th field, absent on an older native lib. `0` (and anything out of range) means "not
        // advertised" → null, and the caller falls back to 47990.
        mgmtPort = if (f.size > 8) f[8].toIntOrNull()?.takeIf { it in 1..65535 } else null,
    )
}

/**
 * Reduce a raw `os` TXT value to the trusted grammar (pf-client-core's `sanitize_os`, mirrored):
 * lowercase slash-separated tokens of `[a-z0-9._-]`, each ≤ 32 chars, at most 5. mDNS is
 * unauthenticated input; a value that sanitizes to nothing becomes "" (no icon, like an older host).
 */
fun sanitizeOsChain(raw: String): String =
    raw.lowercase()
        .split('/')
        .map { token -> token.filter { it in 'a'..'z' || it in '0'..'9' || it in "._-" }.take(32) }
        .filter { it.isNotEmpty() }
        .take(5)
        .joinToString("/")

/**
 * The icon-lookup order for a chain: sanitized tokens most-specific-first, brand aliases applied
 * (`macos` → `apple` art, `steamos` → `steam` art) — pf-client-core's `os_icon_tokens`, mirrored.
 * The UI takes the first token it has art for; empty means "no OS icon" (older host / garbage).
 */
fun osIconTokens(chain: String): List<String> =
    sanitizeOsChain(chain)
        .split('/')
        .filter { it.isNotEmpty() }
        .reversed()
        .map {
            when (it) {
                "macos" -> "apple"
                "steamos" -> "steam"
                else -> it
            }
        }

/**
 * Browses `_punktfunk._udp` for punktfunk/1 hosts via the native `mdns-sd` core (the same browse the
 * Linux/Windows clients use), exposed over JNI — *not* `NsdManager`, whose per-OEM system daemon
 * made discovery "mostly broken". The native browse is polled ~1 Hz on the main thread and the live
 * host set pushed to every listener (also on the main thread, only when it changes).
 *
 * One per process, via [shared]: each instance is one mDNS daemon binding :5353 and joining the
 * multicast groups, so two of them contend for the same answers. Subscribers hold it up —
 * [addListener] starts the browse, and the last one out stops it after a short idle gap. Main
 * thread only.
 *
 * We hold a Wi-Fi [WifiManager.MulticastLock] for the browse lifetime — raw multicast *reception*
 * needs it. (The Android emulator's SLIRP NAT drops multicast, so on the emulator discovery starts
 * but never finds a LAN host — same as before; that's the network, not the API.)
 *
 * The daemon's sockets belong to the interface they were built on. A TV that moves from Wi-Fi to
 * Ethernet never backgrounds the app, so nothing else would rebuild them: the default-network
 * watch below does, and tells [addNetworkListener] subscribers to probe again.
 */
class HostDiscovery private constructor(context: Context) {
    private val appCtx = context.applicationContext

    /** Subscribers, notified on the main thread. The browse runs while this is non-empty. */
    private val listeners = mutableListOf<(List<DiscoveredHost>) -> Unit>()

    /** How many subscribers hold the browse up. Zero means no daemon and no multicast lock. */
    val listenerCount: Int get() = listeners.size

    /** Started activities, counted by [onAppStart]/[onAppStop]. At zero the browse sleeps. */
    private var visible = 0

    /** Whether any activity is on screen: probing for a UI nobody sees is battery for nothing. */
    val appVisible: Boolean get() = visible > 0

    /** An activity started: a browse the app's last stop put to sleep wakes for its subscribers. */
    fun onAppStart() {
        if (visible++ == 0 && listeners.isNotEmpty()) start()
    }

    /**
     * An activity stopped. The last one out ends the browse and its Wi-Fi locks at once;
     * subscribers stay subscribed and get fresh hosts after [onAppStart].
     */
    fun onAppStop() {
        visible = (visible - 1).coerceAtLeast(0)
        if (visible == 0) {
            handler.removeCallbacks(quiesce)
            stop()
        }
    }

    private val handler = Handler(Looper.getMainLooper())
    private var multicastLock: WifiManager.MulticastLock? = null
    private var wifiLocks: List<WifiManager.WifiLock> = emptyList()
    private var nativeHandle = 0L
    private var running = false
    private var last: List<DiscoveredHost> = emptyList()
    /** Failed [start] calls since the last good one; see [MAX_START_ATTEMPTS]. */
    private var attempt = 0
    private val retry = Runnable { start() }

    /** Told, on the main thread, once the device is on a different network. */
    private val networkListeners = mutableListOf<() -> Unit>()

    /** Interface + addresses of the default network as last seen; null before the first one. */
    private var linkSignature: String? = null

    /** The default network went away; whatever comes next is a change, even the same SSID. */
    private var lost = false

    /** The pending [networkChanged], held back while the new link settles. */
    private val settle = Runnable { networkChanged() }

    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onLinkPropertiesChanged(network: Network, lp: LinkProperties) {
            // IPv4 and link-local only: IPv6 privacy addresses rotate on a network that is
            // the same for every purpose this browse has.
            val addrs = lp.linkAddresses
                .filter { it.address is java.net.Inet4Address || it.address.isLinkLocalAddress }
                .map { it.toString() }
                .sorted()
            val sig = lp.interfaceName + "|" + addrs
            val changed = lost || (linkSignature != null && linkSignature != sig)
            linkSignature = sig
            lost = false
            if (changed) scheduleNetworkChanged()
        }

        override fun onLost(network: Network) {
            lost = true
        }
    }

    init {
        val cm = appCtx.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
        runCatching { cm?.registerDefaultNetworkCallback(networkCallback, handler) }
            .onFailure { Log.w(TAG, "default network watch unavailable", it) }
    }

    /** See [removeListener]: tears the browse down once nobody has come back for it. */
    private val quiesce = Runnable { if (listeners.isEmpty()) stop() }

    private val poll = object : Runnable {
        override fun run() {
            if (!running) return
            val hosts = snapshot()
            if (hosts != last) {
                last = hosts
                listeners.toList().forEach { it(hosts) }
            }
            handler.postDelayed(this, POLL_MS)
        }
    }

    /**
     * Subscribe to the live host set, browsing while anyone is subscribed. A listener that joins a
     * browse already holding hosts gets them at once, so a screen opening over one draws its list
     * without waiting for a poll. Registering the same listener twice does nothing.
     */
    fun addListener(listener: (List<DiscoveredHost>) -> Unit) {
        if (listeners.any { it === listener }) return
        handler.removeCallbacks(quiesce)
        listeners += listener
        // A no-op while the browse is still lingering; away, [onAppStart] starts it.
        if (listeners.size == 1 && appVisible) start()
        if (last.isNotEmpty()) listener(last)
    }

    /**
     * Unsubscribe. The browse ends [IDLE_LINGER_MS] after the last listener leaves, which frees the
     * daemon and the multicast lock — the delay is what carries it across a handover, since Compose
     * disposes the screen that is leaving before it composes the one arriving, and a browse rebuilt
     * on every switch between the touch and console homes is the churn this shares an instance to
     * avoid.
     */
    fun removeListener(listener: (List<DiscoveredHost>) -> Unit) {
        listeners.removeAll { it === listener }
        if (listeners.isEmpty()) {
            handler.removeCallbacks(quiesce)
            handler.postDelayed(quiesce, IDLE_LINGER_MS)
        }
    }

    /**
     * Be told when the device lands on a different network — a new interface, or new addresses on
     * the one it had. The browse has already been rebuilt by then; a subscriber uses it to probe
     * its saved hosts at once instead of waiting out its cadence.
     */
    fun addNetworkListener(listener: () -> Unit) {
        if (networkListeners.none { it === listener }) networkListeners += listener
    }

    fun removeNetworkListener(listener: () -> Unit) {
        networkListeners.removeAll { it === listener }
    }

    /** Hold the rebuild until the link stops changing: DHCP lands its lease a beat after link-up. */
    private fun scheduleNetworkChanged() {
        handler.removeCallbacks(settle)
        handler.postDelayed(settle, NETWORK_SETTLE_MS)
    }

    /**
     * The device is on a different network. A running browse is rebuilt on it — its sockets were
     * bound on the old interface — and one that gave up is given its retries back. Then every
     * network subscriber is told, so the presence pips follow within a probe, not a cadence.
     */
    private fun networkChanged() {
        Log.i(TAG, "default network changed — rebuilding the browse")
        restart()
        networkListeners.toList().forEach { it() }
    }

    /**
     * Spin the browse up, retrying a failed daemon on a short cadence, then a slow one. A start
     * can fail for a reason that is over in a second — its :5353 bind losing a race with the
     * daemon we just tore down — and one that stays failed is retried for as long as anyone is
     * subscribed: a TV has no foreground/background trip to ask again with.
     */
    private fun start() {
        if (running) return
        handler.removeCallbacks(retry)
        acquireMulticastLock()
        val h = runCatching { NativeBridge.nativeDiscoveryStart() }
            .onFailure { Log.e(TAG, "nativeDiscoveryStart threw", it) }
            .getOrDefault(0L)
        if (h == 0L) {
            releaseMulticastLock()
            attempt++
            val wait = if (attempt <= MAX_START_ATTEMPTS) RETRY_MS else SLOW_RETRY_MS
            Log.w(TAG, "native mDNS discovery did not start — retry $attempt in $wait ms")
            handler.postDelayed(retry, wait)
            return
        }
        attempt = 0
        nativeHandle = h
        running = true
        last = emptyList()
        handler.post(poll)
    }

    /**
     * Ask again, keeping the daemon: `mdns-sd` re-queries on a doubling backoff that caps at an
     * hour, so a long-lived browse is effectively passive — a host that appeared since, or whose
     * announcement was lost to multicast, may never be asked for again. This is the default
     * refresh; it costs one PTR and keeps the sockets, the multicast memberships and the cache.
     *
     * A browse that is not running is built instead, since there is nothing to ask with.
     */
    fun rescan() {
        val h = nativeHandle
        if (!running || h == 0L) {
            restart()
            return
        }
        runCatching { NativeBridge.nativeDiscoveryRescan(h) }
            .onFailure { Log.e(TAG, "nativeDiscoveryRescan threw", it) }
    }

    /**
     * Tear the browse down and build a fresh one. Only for a browse whose sockets are wrong rather
     * than merely quiet — one that started before the local-network grant, so its sends were
     * refused and its group joins never took. [rescan] covers every other refresh: a rebuild
     * re-binds :5353 and re-joins the groups, and one that fails leaves the device blind.
     *
     * The shown host set is left alone across the swap; the first poll of the new browse
     * publishes the fresh one. A browse nobody holds up is not rebuilt: a grant that lands
     * mid-stream must not put a daemon beside the session, with nothing left to stop it. Nor is
     * one rebuilt while the app is away: [onAppStart] builds it on the way back.
     */
    fun restart() {
        stop()
        if (listeners.isNotEmpty() && appVisible) start()
    }

    private fun stop() {
        handler.removeCallbacks(retry)
        attempt = 0 // the next start is a new lifetime, with its own retries
        if (!running && nativeHandle == 0L) return
        running = false
        handler.removeCallbacks(poll)
        val h = nativeHandle
        nativeHandle = 0L
        if (h != 0L) runCatching { NativeBridge.nativeDiscoveryStop(h) }
            .onFailure { Log.e(TAG, "nativeDiscoveryStop threw", it) }
        releaseMulticastLock()
        last = emptyList()
    }

    private fun snapshot(): List<DiscoveredHost> {
        val h = nativeHandle
        if (h == 0L) return emptyList()
        // getOrNull (not getOrDefault): the JNI returns a platform String!, so a (near-impossible)
        // native null is a *success* value here — coalesce it so the main-thread poll can't NPE.
        val blob = runCatching { NativeBridge.nativeDiscoveryPoll(h) }
            .onFailure { Log.e(TAG, "nativeDiscoveryPoll threw", it) }
            .getOrNull() ?: ""
        if (blob.isEmpty()) return emptyList()
        return blob.split('\n')
            .filter { it.isNotBlank() }
            .mapNotNull { parseHostRecord(it) }
            .associateBy { it.key } // dedup by stable key (id, or addr:port)
            .values
            .sortedWith(nameOrder { it.name })
    }

    private fun acquireMulticastLock() {
        // Wi-Fi only: an Ethernet-only box has no WifiManager, and needs no lock to receive.
        val wifi = appCtx.getSystemService(Context.WIFI_SERVICE) as? WifiManager ?: return
        multicastLock = wifi.createMulticastLock("punktfunk-mdns").apply {
            setReferenceCounted(true)
            runCatching { acquire() }
        }
        // The MulticastLock unblocks the filter but leaves Wi-Fi power save on. A power-saving
        // client misses the multicast IGMP queries a snooping AP sends, is pruned, and stops
        // getting the group — the stream holds these same locks to stay awake, so does the browse.
        wifiLocks = buildList {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "punktfunk:mdns-ll")?.let(::add)
            }
            @Suppress("DEPRECATION")
            wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "punktfunk:mdns-hp")?.let(::add)
        }.onEach { it.setReferenceCounted(false); runCatching { it.acquire() } }
    }

    private fun releaseMulticastLock() {
        multicastLock?.takeIf { it.isHeld }?.let { runCatching { it.release() } }
        multicastLock = null
        wifiLocks.forEach { l -> l.takeIf { it.isHeld }?.let { runCatching { it.release() } } }
        wifiLocks = emptyList()
    }

    companion object {
        private const val POLL_MS = 1000L
        private const val RETRY_MS = 2000L
        private const val IDLE_LINGER_MS = 3000L
        private const val MAX_START_ATTEMPTS = 5
        private const val SLOW_RETRY_MS = 30_000L
        private const val NETWORK_SETTLE_MS = 1500L

        @Volatile
        private var instance: HostDiscovery? = null

        /**
         * The process's one browse. Every caller shares it: a second instance is a second mDNS
         * daemon on :5353 splitting the same answers with the first.
         */
        fun shared(context: Context): HostDiscovery =
            instance ?: synchronized(this) {
                instance ?: HostDiscovery(context).also { instance = it }
            }
    }
}
