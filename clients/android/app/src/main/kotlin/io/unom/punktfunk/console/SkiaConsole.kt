package io.unom.punktfunk.console

import android.app.ActivityManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.InputDevice
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import io.unom.punktfunk.CONNECT_TIMEOUT_MS
import io.unom.punktfunk.ConnectErrors
import io.unom.punktfunk.HostActions
import io.unom.punktfunk.PresetStore
import io.unom.punktfunk.Settings
import io.unom.punktfunk.SettingsStore
import io.unom.punktfunk.SpeedTestPhase
import io.unom.punktfunk.StreamPreset
import io.unom.punktfunk.connectToHost
import io.unom.punktfunk.deviceName
import io.unom.punktfunk.effectiveFor
import io.unom.punktfunk.matches
import io.unom.punktfunk.runSpeedTest
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.discovery.Presence
import io.unom.punktfunk.kit.discovery.PresenceTracker
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.link.StartScreen
import io.unom.punktfunk.kit.link.host
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.LibraryResult
import io.unom.punktfunk.kit.library.RunningGame
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.security.obtainIdentity
import io.unom.punktfunk.models.ActiveSession
import java.io.File
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import okhttp3.Cache
import okhttp3.CacheControl
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONArray
import org.json.JSONObject

/**
 * The Skia console (`crates/pf-console-ui`, drawn by native over EGL — design
 * `android-skia-console-port.md`) as this app holds it: ONE instance for the process, created
 * lazily and never torn down while the app lives, so the console's screen stack survives a trip
 * through the stream exactly as the desktop's does (the shelf is where you left it when the game
 * exits). [SkiaConsoleShell] attaches a surface, the pad probes and the overlays to it while the
 * console is on screen; between, it idles parked.
 *
 * This object is the SERVICE side of the console's model (`ConsoleShared` / `LibraryShared` /
 * `ConsoleBus`): it feeds host rows from the trust store + discovery + the reachability probe,
 * runs the library fetch/cache/art pipeline, pairing, wake-and-wait, and the settings round-trip,
 * and turns the console's own asks (`OverlayAction`) into a connect, a clipboard write, or a
 * task-to-back. Everything blocking runs on its own executor; every native call is cheap.
 */
object SkiaConsole {
    private const val TAG = "pf.console"

    /**
     * On-glass triage switch: `adb shell setprop debug.punktfunk.console_backend none` makes the
     * app behave as if the native console host were absent (the touch UI fronts everything, a
     * controller drives it through Compose focus). Anything else = the console.
     */
    private const val BACKEND_PROP = "debug.punktfunk.console_backend"

    /** Where the console-owned settings keys (`library_view`, `reduce_motion`, …) persist. */
    private const val PREFS = "punktfunk_console_settings"

    private var handle = 0L

    /**
     * False once the console has proven it cannot draw — the native create failed, or the render
     * thread died (a GL context that never came up, or one Android reclaimed and that would not
     * come back). Compose observes it: `App` folds it into the gamepad-UI gate, so the answer to a
     * dead console is the touch UI — not the gray, never-painted `SurfaceView` the shell would
     * otherwise sit on for the rest of the process.
     */
    var healthy by mutableStateOf(true)
        private set

    private var appContext: Context? = null
    private val main = Handler(Looper.getMainLooper())
    private val ioPool = Executors.newCachedThreadPool { r -> Thread(r, "pf-console-io").apply { isDaemon = true } }
    private val artPool = Executors.newFixedThreadPool(3) { r -> Thread(r, "pf-console-art").apply { isDaemon = true } }
    /** One disk cache behind both art clients: the host proxy sends `Cache-Control` + `ETag`, a
     *  CDN its own, and OkHttp honours either, so a shelf revisit is a 304 at most. Null before
     *  `init`: no context, no cache, fetches still work. */
    private val artCache: Cache? by lazy { appContext?.let { Cache(File(it.cacheDir, "art-http"), 64L shl 20) } }
    private val artHttp by lazy { OkHttpClient.Builder().cache(artCache).build() }
    private var eventThread: Thread? = null
    private val running = AtomicBoolean(false)

    // Services.
    private lateinit var knownHostStore: KnownHostStore
    private lateinit var presetStore: PresetStore
    private lateinit var settingsStore: SettingsStore
    private var identity: ClientIdentity? = null
    /** The identity load has ended, with or without one. Main-thread only. */
    private var identityLoaded = false
    private var discovery: HostDiscovery? = null
    private var discovered: List<DiscoveredHost> = emptyList()

    /** Record ids that answered the probe — the whole of presence. See [sweep]. */
    private var reachable: Set<String> = emptySet()
    private val presence = PresenceTracker()
    private var settings: Settings = Settings()

    /** What each paired host last said this device may do TO it, by fingerprint, and when we
     *  last asked — the Android half of the desktop's shared actions cache. Main-thread only. */
    private val hostActions = mutableMapOf<String, List<HostActions.Action>>()
    private val hostActionsAt = mutableMapOf<String, Long>()

    /** What each paired host has UP, by fingerprint, and when we last asked — the same shape
     *  on a much shorter fuse (`pf_client_core::library::RUNNING_TTL`). Main-thread only. */
    private val nowPlaying = mutableMapOf<String, String>()
    private val nowPlayingAt = mutableMapOf<String, Long>()

    // What the composable hands us while it is on screen.
    private var onConnected: ((ActiveSession) -> Unit)? = null

    /**
     * The connected session the console's launch hold is still standing in front of.
     *
     * Handed to the app on `ShowStream`, dropped on a cancel. Null whenever the console is not
     * holding one — a desktop-session connect, or a launch the shell decided not to hold (a
     * launcher tile, which the host never tracks).
     */
    private var pendingSession: ActiveSession? = null

    /** Whether the launch just dialled is one the shell will hold: a game, not a launcher. */
    private var holdsLaunch = false
    private var onSettingsChange: ((Settings) -> Unit)? = null
    private var onQuit: (() -> Unit)? = null
    private var onPlatformScreen: ((String) -> Unit)? = null
    private var onPadAction: ((String, String) -> Unit)? = null
    private var onPulse: ((String) -> Unit)? = null

    /** The console's focus, in words, whenever it changes — the shell speaks it to TalkBack. */
    private var onAnnounce: ((String) -> Unit)? = null

    /** The connect in flight, if any — cancelable through `OverlayAction::CancelConnect`. */
    private class Dial(val cancelled: AtomicBoolean = AtomicBoolean(false))
    private var dial: Dial? = null

    /** The wake-and-wait loop in flight, if any. */
    private var wakeGen = AtomicLong(0)

    /** The library fetch in flight (its generation; a newer one supersedes it). */
    private val fetchGen = AtomicLong(0)

    // ---- availability -------------------------------------------------------------------

    /**
     * Whether the console can front the gamepad UI on this device: the native host must be in
     * this build (every shipping ABI today — see `nativeConsoleAvailable`) and the triage sysprop
     * must not say `none`.
     */
    fun wanted(): Boolean {
        val available = runCatching { NativeBridge.nativeConsoleAvailable() }.getOrDefault(false)
        if (!available) return false
        return backendProp() != "none"
    }

    /**
     * Why the console cannot front the gamepad UI here, or null when it can — for the settings
     * screen to print under the switch that asks for it.
     *
     * `App` gates the console on `wanted() && healthy` on top of the user's own setting, and those
     * two terms are the ONLY ones that can veto "Always": the mode, the attached pad, the TV check
     * and the dev flag are ORed together, so a device where the console never comes up ignores
     * every one of them. Until this existed that produced a switch the app silently disobeyed —
     * indistinguishable, from the outside, from the switch itself being broken, and it is what a
     * report of "the gamepad UI just doesn't activate, even on Always, even with a controller"
     * looks like. Reads [healthy] as Compose state, so the note clears itself if it ever recovers.
     */
    fun unavailable(): String? = when {
        !wanted() -> "This device has no console UI in this build, so the touch layout stays up."
        !healthy -> "The console UI couldn't start on this device, so the touch layout is " +
            "standing in. Restart the app to try again — and if it keeps happening, send this " +
            "host your logs from a saved host's ⋮ menu."
        else -> null
    }

    private fun backendProp(): String = runCatching {
        val cls = Class.forName("android.os.SystemProperties")
        cls.getMethod("get", String::class.java, String::class.java)
            .invoke(null, BACKEND_PROP, "") as String
    }.getOrDefault("").trim().lowercase()

    // ---- lifecycle -----------------------------------------------------------------------

    /**
     * Build the console if it does not exist yet. Idempotent; call from the main thread. Returns
     * the native handle (`0` = the console could not be built; the caller keeps the Compose
     * console).
     */
    /**
     * @param pendingLink true when a `punktfunk://` URL is waiting to be routed. Explicit intent
     *   beats the start-screen policy, and the link is handled after composition — so the console
     *   must not open a shelf first and make the link the second thing that happens.
     */
    fun ensure(context: Context, initial: Settings, pendingLink: Boolean = false): Long {
        if (handle != 0L) return handle
        val app = context.applicationContext
        appContext = app
        knownHostStore = KnownHostStore(app)
        presetStore = PresetStore(app)
        settingsStore = SettingsStore(app)
        settings = initial
        val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val base = prefs.getString("json", null)?.let { runCatching { JSONObject(it) }.getOrNull() }
        val presets = presetStore.all()
        val opts = JSONObject()
            .put("device_name", deviceName(app))
            .put("gpu_cache_bytes", gpuCacheBytes(app))
            // The touch shell exists as a fallback on phones/tablets but not on a TV —
            // gates the console's own "Controller-optimized UI" off switch.
            .put("fallback_ui", !io.unom.punktfunk.isTvDevice(app))
            // The same MediaCodec answer the Hello advertises by: without a real AV1
            // decoder the codec row marks AV1 unsupported instead of offering a dead pick.
            .put("av1_ok", VideoDecoders.decodableCodecBits() and 4 != 0)
            .put("settings", ConsoleJson.settings(initial, base))
            .put("presets", JSONArray(ConsoleJson.presets(presets)))
            .put("known_hosts", JSONObject(ConsoleJson.knownHosts(knownHostStore.all())))
            .put("entry", startEntry(initial, pendingLink, presets))
        // A phone's own shape leads the Aspect row; a TV's panel is a standard one.
        if (!io.unom.punktfunk.isTvDevice(app)) {
            val (nw, nh, _) = io.unom.punktfunk.nativeDisplayMode(app)
            val (sw, sh, _) = io.unom.punktfunk.safeDisplayMode(app)
            opts.put("screen", JSONArray(listOf(nw, nh))).put("safe_area", JSONArray(listOf(sw, sh)))
        }
        handle = runCatching { NativeBridge.nativeConsoleCreate(opts.toString()) }.getOrDefault(0L)
        if (handle == 0L) {
            Log.e(TAG, "console: native create failed")
            healthy = false // see [healthy] — the touch UI fronts everything from here
            return 0L
        }
        Log.i(TAG, "console: created (gpu cache ${gpuCacheBytes(app) shr 20} MB)")
        startEventThread()
        startServices(app)
        return handle
    }

    /**
     * The console's entry screen: `{}` for the host list, `{"library": row}` for the default
     * host's shelf, `{"stream": row}` to also dial its desktop. Once per process — this runs
     * inside [ensure], which returns early on every later call.
     */
    private fun startEntry(
        s: Settings,
        pendingLink: Boolean,
        presets: List<StreamPreset>,
    ): JSONObject {
        if (pendingLink) return JSONObject()
        val hosts = knownHostStore.all()
        val start = StartScreen.resolve(s.startIn, s.defaultHost, hosts)
        val host = start.host ?: return JSONObject()
        Log.i(TAG, "console start: start_in=${s.startIn} default=${host.name}")
        val row = ConsoleJson.hostRow(host, null, presets)
        return JSONObject().put(if (start is StartScreen.Stream) "stream" else "library", row)
    }

    /**
     * Skia's resource budget: a quarter of the desktop's 160 MB on a ≤ 2 GB box, the desktop
     * figure above (design D11 — a 160 MB texture cache is how a TV box gets its process killed).
     */
    private fun gpuCacheBytes(context: Context): Int {
        val am = context.getSystemService(Context.ACTIVITY_SERVICE) as? ActivityManager
        val classMb = am?.memoryClass ?: 128
        return if (classMb >= 256) 160 shl 20 else 64 shl 20
    }

    /** Held as one value so [resumeDiscovery] and [pauseDiscovery] name the same subscriber. */
    private val onDiscovered: (List<DiscoveredHost>) -> Unit = { list ->
        discovered = list
        // Learn wake MACs / mgmt ports from live adverts, as the desktop service does.
        ioPool.execute {
            var changed = false
            for (dh in list) {
                val kh = knownHostStore.all().firstOrNull { it.matches(dh) } ?: continue
                if (dh.mac.isNotEmpty() && dh.mac.toSet() != kh.mac.toSet()) {
                    knownHostStore.learnMac(kh, dh.mac); changed = true
                }
                dh.mgmtPort?.let { if (it != kh.mgmtPort) { knownHostStore.learnMgmtPort(kh, it); changed = true } }
                if (dh.os.isNotEmpty() && dh.os != kh.os) { knownHostStore.learnOs(kh, dh.os); changed = true }
            }
            main.post { pushHosts(); if (changed) pushKnownHosts() }
        }
        pushHosts()
    }

    /**
     * Subscribe to the shared browse, ask it for a fresh answer and probe every saved host now —
     * the console is back on screen, or a dial that was holding the radio let go. Paired with
     * [pauseDiscovery], which drops the subscription; the browse itself ends only once nobody has
     * claimed it back.
     */
    private fun resumeDiscovery() {
        discovery?.let {
            it.addListener(onDiscovered)
            it.rescan()
        }
        sweepNow()
    }

    /** Let the browse go, so the radio belongs to the stream that is about to start. */
    private fun pauseDiscovery() {
        discovery?.removeListener(onDiscovered)
    }

    /** The device landed on another network: the browse was rebuilt, now ask the hosts. */
    private val onNetworkChanged: () -> Unit = { sweepNow() }

    /**
     * The reachability sweep — the whole of presence, every ~12 s (the desktop's cadence), and at
     * once on [sweepNow]. Every saved host, including the ones on mDNS: an advert is a cache entry
     * with a 75-minute TTL that a suspending host sends no goodbye for, so trusting it left a
     * sleeping machine reading Online and, since Wake is gated on `!online`, unwakeable.
     *
     * Only while the console is ON SCREEN (attached): parked behind the touch UI or a stream
     * there is nobody to show the pips to — and mid-stream the radio belongs to the session. The
     * timer keeps ticking so probes resume within a cadence of re-attach.
     */
    private val sweep = object : Runnable {
        override fun run() {
            if (handle == 0L) return
            main.removeCallbacks(this)
            main.postDelayed(this, SWEEP_MS)
            if (onConnected == null) return
            val saved = knownHostStore.all()
            val live = discovered
            ioPool.execute {
                val up = Presence.sweep(
                    saved,
                    liveFor = { kh -> live.firstOrNull { kh.matches(it) } },
                    probe = { addr, port -> NativeBridge.nativeProbe(addr, port, Presence.PROBE_MS) },
                )
                // A pinned host that answered somewhere else has moved: follow it, so the dial
                // and the library fetch go where it lives.
                val moved = saved.any { kh -> up[kh.id]?.let { knownHostStore.learnAddress(kh.fpHex, it.address, it.port) } == true }
                main.post {
                    val next = presence.apply(saved.map { it.id }.toSet(), up.keys)
                    if (next != reachable || moved) {
                        reachable = next
                        pushHosts()
                        if (moved) pushKnownHosts()
                    }
                }
            }
        }
    }

    /** Probe now rather than at the next tick; the cadence restarts from here. */
    private fun sweepNow() {
        main.removeCallbacks(sweep)
        main.post(sweep)
    }

    private fun startServices(app: Context) {
        ioPool.execute {
            val id = runCatching { obtainIdentity(IdentityStore(app)) }
                .onFailure { Log.w(TAG, "identity unavailable: ${it.message}") }
                .getOrNull()
            main.post { identity = id; identityLoaded = true }
        }
        discovery = HostDiscovery.shared(app).also { it.addNetworkListener(onNetworkChanged) }
        resumeDiscovery()
        // Commands from the console, drained on a short cadence once the identity load ends:
        // a start entry queues its shelf fetch or desktop dial before that.
        main.post(object : Runnable {
            override fun run() {
                if (handle == 0L) return
                if (identityLoaded) drainCommands()
                main.postDelayed(this, 100)
            }
        })
        pushHosts()
    }

    private fun startEventThread() {
        running.set(true)
        eventThread = Thread({
            while (running.get() && handle != 0L) {
                val json = runCatching { NativeBridge.nativeConsoleNextEvent(handle) }.getOrDefault("")
                if (json.isEmpty()) continue
                val ev = runCatching { JSONObject(json) }.getOrNull() ?: continue
                main.post { onEvent(ev) }
            }
        }, "pf-console-events").apply { isDaemon = true; start() }
    }

    // ---- what the composable attaches ------------------------------------------------------

    fun attach(
        onConnected: (ActiveSession) -> Unit,
        onSettingsChange: (Settings) -> Unit,
        onQuit: () -> Unit,
        onPlatformScreen: (String) -> Unit,
        onPadAction: (String, String) -> Unit,
        onPulse: (String) -> Unit,
        onAnnounce: (String) -> Unit,
    ) {
        this.onConnected = onConnected
        this.onSettingsChange = onSettingsChange
        this.onQuit = onQuit
        this.onPlatformScreen = onPlatformScreen
        this.onPadAction = onPadAction
        this.onPulse = onPulse
        this.onAnnounce = onAnnounce
        resumeDiscovery()
        // The touch UI may have paired/forgotten/edited hosts or presets while we were away.
        pushHosts()
        pushKnownHosts()
        if (handle != 0L) NativeBridge.nativeConsoleSetPresets(handle, ConsoleJson.presets(presetStore.all()))
    }

    fun detach() {
        // Parked behind the touch UI, which runs the browse itself: hold it here too and the two
        // screens would both be subscribers, so neither could ever quiet it for a measurement.
        pauseDiscovery()
        onConnected = null
        onSettingsChange = null
        onQuit = null
        onPlatformScreen = null
        onPadAction = null
        onPulse = null
        onAnnounce = null
    }

    /** The touch UI (or a link) changed settings: the console reads the new snapshot next. */
    fun settingsChanged(s: Settings) {
        settings = s
        if (handle == 0L) return
        val prefs = appContext?.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val base = prefs?.getString("json", null)?.let { runCatching { JSONObject(it) }.getOrNull() }
        NativeBridge.nativeConsoleSetSettings(handle, ConsoleJson.settings(s, base).toString())
    }

    /** The preset catalog changed (the touch settings edited it). */
    fun presetsChanged() {
        if (handle == 0L) return
        NativeBridge.nativeConsoleSetPresets(handle, ConsoleJson.presets(presetStore.all()))
        pushHosts()
    }

    /** The host store changed outside the console (touch UI pairing / forget). */
    fun hostsChanged() {
        if (handle == 0L) return
        pushHosts()
        pushKnownHosts()
    }

    /** A session the console started (or any session) has ended; [reason] = the abnormal one. */
    fun sessionEnded(reason: String?) {
        if (handle == 0L) return
        NativeBridge.nativeConsoleSessionPhase(handle, 3, reason.orEmpty())
        resumeDiscovery()
    }

    /** Re-root the console on a host's shelf (a game launched from it just exited; a deep link). */
    fun openLibrary(hostId: String, pinId: String?) {
        if (handle == 0L) return
        val kh = knownHostStore.byId(hostId) ?: return
        val presets = presetStore.all()
        val pin = pinId?.let { id -> presets.firstOrNull { it.id == id } }
        val entry = JSONObject().put("library", ConsoleJson.hostRow(kh, pin, presets))
        NativeBridge.nativeConsoleNavigate(handle, entry.toString())
    }

    /**
     * A `punktfunk://` link while the console is up. Named-by-id and pinned is the one-click
     * contract (the same dial the console's own Launch takes); anything that would need a trust
     * decision — or that named the host by a guessable label or address — is a notice here. A link
     * may never establish trust, the console's Pair screen is reached from the host's tile rather
     * than from a URL, and the console draws no prompt this shell could ask a question through.
     */
    fun handleDeepLink(url: String) {
        if (handle == 0L) return
        val parsed = io.unom.punktfunk.kit.link.DeepLinks.parse(url)
        if (parsed is io.unom.punktfunk.kit.link.DeepLinkResult.Refused) {
            if (parsed.error != io.unom.punktfunk.kit.link.LinkError.NOT_OUR_SCHEME) notice(parsed.message())
            return
        }
        val link = (parsed as io.unom.punktfunk.kit.link.DeepLinkResult.Parsed).link
        if (link.route != io.unom.punktfunk.kit.link.LinkRoute.CONNECT) {
            notice("Punktfunk on Android can't do “${link.route.word}” links yet.")
            return
        }
        val presetRef = link.preset
        if (presetRef != null) {
            val (_, resolution) = presetStore.resolve(presetRef)
            if (resolution != io.unom.punktfunk.PresetResolution.FOUND) {
                notice("That link asks for a preset called “$presetRef”, which isn't on this device.")
                return
            }
        }
        when (val resolved = io.unom.punktfunk.kit.link.DeepLinks.resolveHost(link, knownHostStore.all())) {
            is io.unom.punktfunk.kit.link.HostResolution.Record -> {
                val kh = resolved.host
                if (link.pinConflict(kh)) {
                    notice("That link's fingerprint doesn't match the one pinned for ${kh.name}.")
                    return
                }
                if (kh.fpHex.isEmpty() || !kh.paired) {
                    notice("Pair with ${kh.name} first — a link can't establish trust.")
                    return
                }
                if (resolved is io.unom.punktfunk.kit.link.HostResolution.Confirm) {
                    notice("A link can only dial ${kh.name} by its id — open it from the list.")
                    return
                }
                launch(
                    JSONObject()
                        .put("addr", kh.address).put("port", kh.port).put("fp_hex", kh.fpHex)
                        .put("launch", link.launch ?: JSONObject.NULL)
                        .put("preset", presetRef?.let { presetStore.resolve(it).first?.id } ?: JSONObject.NULL)
                        .put("request_access", false),
                )
            }
            is io.unom.punktfunk.kit.link.HostResolution.Unknown ->
                notice("That link points at a host this device hasn't paired with.")
            io.unom.punktfunk.kit.link.HostResolution.Ambiguous ->
                notice("More than one saved host is called “${link.hostRef}”.")
            io.unom.punktfunk.kit.link.HostResolution.Unresolvable ->
                notice("That link points at a host this device doesn't know.")
        }
    }

    /** The connected controllers, for the chip + settings rows, plus pads with no `InputDevice`. */
    internal fun padsChanged(driving: InputDevice?, extras: List<ConsoleJson.ExtraPad> = emptyList()) {
        if (handle == 0L) return
        NativeBridge.nativeConsoleSetPads(
            handle,
            ConsoleJson.pads(Gamepad.pads(), driving ?: Gamepad.firstPad(), extras),
        )
    }

    // ---- model pushers -----------------------------------------------------------------------

    private fun pushHosts() {
        if (handle == 0L) return
        refreshHostState()
        NativeBridge.nativeConsoleSetHosts(
            handle,
            ConsoleJson.hostRows(
                knownHostStore.all(), discovered, reachable, presetStore.all(), hostActions,
                nowPlaying,
            ),
        )
    }

    /**
     * Keep each paired, reachable host's advertised actions and running title fresh, mirroring
     * the desktop's `Service::refresh_host_state`.
     *
     * Actions run on a slow TTL and never when a menu opens: the row list has to be SETTLED
     * before the menu draws, or rows would appear under a cursor already moving toward
     * something else — and two of those rows shut a machine down. What a host has UP changes
     * between two visits to the carousel, so it gets its own, far shorter one.
     */
    private fun refreshHostState() {
        val id = identity ?: return
        val now = android.os.SystemClock.elapsedRealtime()
        for (h in knownHostStore.all()) {
            if (!h.paired || h.fpHex.isEmpty()) continue
            // Reachable means it answered the probe — an advert would say yes for a host that
            // is asleep, and this asks it a question only a live host can answer.
            if (h.id !in reachable) continue
            val (addr, mgmt, fp) = Triple(h.address, h.effectiveMgmtPort, h.fpHex)
            // Stamp BEFORE the request, so a slow host cannot make every push spawn another.
            if (now - (nowPlayingAt[fp] ?: 0L) >= NOW_PLAYING_TTL_MS) {
                nowPlayingAt[fp] = now
                ioPool.execute {
                    val up = LibraryClient.fetchRunning(addr, mgmt, id.certPem, id.privateKeyPem, fp)
                    main.post { recordNowPlaying(fp, up) }
                }
            }
            if (now - (hostActionsAt[fp] ?: 0L) < HOST_ACTIONS_TTL_MS) continue
            hostActionsAt[fp] = now
            ioPool.execute {
                val found = HostActions.list(id, addr, mgmt, fp)
                main.post {
                    hostActions[fp] = found
                    pushHosts()
                }
            }
        }
    }

    /**
     * Adopt a `/status` answer as the carousel's "▶ <title>" line: the first entry that is up
     * and has a title to show, else nothing. Main thread; re-pushes only on a real change, so a
     * host answering every 20 s does not churn the snapshot generation.
     */
    private fun recordNowPlaying(fpHex: String, games: List<RunningGame>) {
        val title = games.firstOrNull { it.isUp && it.title.isNotEmpty() }?.title.orEmpty()
        if (nowPlaying[fpHex] == title) return
        nowPlaying[fpHex] = title
        pushHosts()
    }

    private fun pushKnownHosts() {
        if (handle == 0L) return
        NativeBridge.nativeConsoleSetKnownHosts(handle, ConsoleJson.knownHosts(knownHostStore.all()))
    }

    internal fun notice(text: String) {
        if (handle != 0L) NativeBridge.nativeConsoleNotice(handle, text)
    }

    // ---- events from the console ---------------------------------------------------------

    private fun onEvent(ev: JSONObject) {
        when {
            ev.has("action") -> onAction(ev.get("action"))
            ev.has("pulse") -> onPulse?.invoke(ev.optString("pulse"))
            ev.has("editing") -> {} // the shell draws its own keyboard; nothing to raise here
            // The Skia surface has no accessibility node tree, so the focused row is spoken
            // instead. Already de-duplicated by the render thread: this fires only on a change.
            ev.has("announce") -> onAnnounce?.invoke(ev.optString("announce"))
            ev.has("settings") -> onSettingsSaved(ev.getJSONObject("settings"))
            ev.has("gles") -> Log.i(TAG, "console: GLES ${ev.optInt("gles")}")
            ev.has("dead") -> {
                Log.e(TAG, "console: render thread died: ${ev.optString("dead")}")
                healthy = false // the touch UI takes over; only a process restart tries again
            }
        }
    }

    private fun onSettingsSaved(j: JSONObject) {
        appContext?.getSharedPreferences(PREFS, Context.MODE_PRIVATE)?.edit()
            ?.putString("json", j.toString())?.apply()
        val next = ConsoleJson.applySettings(settings, j)
        if (next != settings) {
            settings = next
            settingsStore.save(next)
            onSettingsChange?.invoke(next)
        }
    }

    private fun onAction(action: Any) {
        when (action) {
            is String -> when (action) {
                "Quit" -> onQuit?.invoke()
                "CancelConnect" -> {
                    dial?.cancelled?.set(true)
                    dial = null
                    // A cancel after the dial landed still has a session to let go of.
                    pendingSession?.let { s -> ioPool.execute { NativeBridge.nativeClose(s.handle) } }
                    pendingSession = null
                    resumeDiscovery()
                }
                // The console's launch hold is done — the game is up, or the player asked to
                // see. Only now does the stream view replace it.
                "ShowStream" -> pendingSession?.let { s ->
                    pendingSession = null
                    onConnected?.invoke(s)
                }
            }
            is JSONObject -> {
                action.optJSONObject("Launch")?.let(::launch)
                action.optString("CopyText").takeIf { action.has("CopyText") }?.let { text ->
                    val cm = appContext?.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
                    cm?.setPrimaryClip(ClipData.newPlainText("punktfunk", text))
                }
            }
        }
    }

    /**
     * `OverlayAction::Launch` — the console asked for a session. The trust decision was the
     * console's (an unpaired host went to its Pair screen first), so this is the dial itself:
     * pinned by the row's fingerprint, with the host's bound preset or the pinned card's
     * one-off, and — for the pair screen's "Request access" — the long approval budget.
     */
    private fun launch(a: JSONObject) {
        val app = appContext ?: return
        val fp = a.optString("fp_hex")
        // The record the row's pin names — never the other OS saved at the same address — and
        // dialled where IT says: the last sweep may have followed the host to a new address. An
        // unpinned row is the placeholder at its address.
        val kh = knownHostStore.resolve(fp, a.optString("addr"), a.optInt("port"))
        val addr = kh?.address ?: a.optString("addr")
        val port = kh?.port ?: a.optInt("port")
        val launchId = a.optString("launch").takeIf { a.has("launch") && !a.isNull("launch") && it.isNotEmpty() }
        val presetId = a.optString("preset").takeIf { a.has("preset") && !a.isNull("preset") && it.isNotEmpty() }
        val requestAccess = a.optBoolean("request_access", false)
        val id = identity
        if (id == null) {
            NativeBridge.nativeConsoleSessionPhase(handle, 2, "Identity not ready yet — try again in a moment")
            return
        }
        // The shell raises its hold for a GAME launch off a shelf; a desktop connect and a
        // launcher tile go straight through, so the session is handed over at once. Mirrors
        // `Shell::launch_hold`, and reads the same cached catalog the shelf was drawn from.
        holdsLaunch = launchId != null &&
            LibraryCache.standard(app.cacheDir).load(kh?.id ?: fp)?.games
                ?.firstOrNull { it.id == launchId }?.isLauncher == false
        val preset: StreamPreset? = presetStore.resolveFor(kh, presetId, launchId)
        val effective = settings.effectiveFor(preset)
        val d = Dial()
        dial = d
        NativeBridge.nativeConsoleSessionPhase(handle, 0, "")
        pauseDiscovery() // the Wi-Fi radio belongs to the stream session now
        ioPool.execute {
            val timeout = if (requestAccess) REQUEST_ACCESS_TIMEOUT_MS else CONNECT_TIMEOUT_MS
            val h = kotlinx.coroutines.runBlocking {
                connectToHost(
                    app, effective, id, addr, port, fp, launchId,
                    dialer = if (launchId != null) "console/library" else "console/desktop",
                    timeoutMs = timeout,
                )
            }
            main.post {
                if (d.cancelled.get()) {
                    if (h != 0L) ioPool.execute { NativeBridge.nativeClose(h) }
                    return@post
                }
                dial = null
                if (h != 0L) {
                    var record = kh
                    // A request-access approval, or a first TOFU-less connect: save the host as
                    // PAIRED, pinning what it presented, so the next connect is silent.
                    if (record == null || (requestAccess && !record.paired)) {
                        val seen = NativeBridge.nativeHostFingerprint(h)
                        if (seen.isNotEmpty()) {
                            val name = record?.name
                                ?: discovered.firstOrNull { it.host == addr && it.port == port }?.name
                                ?: addr
                            record = knownHostStore.trust(addr, port, name, seen, paired = requestAccess || record?.paired == true)
                            pushHosts(); pushKnownHosts()
                        }
                    }
                    if (record != null) {
                        NativeBridge.nativeHostMgmtPort(h).takeIf { it > 0 }?.let {
                            knownHostStore.learnMgmtPort(record, it)
                        }
                    }
                    val session = ActiveSession(
                        h,
                        effective,
                        clipboardSync = record?.clipboardSync ?: false,
                        presetName = preset?.name,
                        hostId = record?.id,
                        launchedFromLibrary = launchId != null,
                        libraryPresetId = presetId,
                    )
                    // The console learns the dial landed and keeps the screen: its launch hold
                    // is still waiting on the game. Handing the session over here instead would
                    // swap the console for the stream view mid-wait, which is the seam this
                    // whole screen exists to remove. `ShowStream` releases it.
                    NativeBridge.nativeConsoleSessionPhase(handle, 1, "")
                    val take = onConnected
                    when {
                        holdsLaunch -> pendingSession = session
                        take != null -> take(session)
                        // Nothing on screen to hand it to (the console is parked behind a
                        // stream): close it rather than leave the host feeding a session
                        // nobody will ever see.
                        else -> ioPool.execute { NativeBridge.nativeClose(h) }
                    }
                } else {
                    val token = NativeBridge.nativeTakeLastError()
                    NativeBridge.nativeConsoleSessionPhase(
                        handle, 2, ConnectErrors.connectMessage(token, requestAccess),
                    )
                    resumeDiscovery()
                }
            }
        }
    }

    // ---- commands from the console -----------------------------------------------------

    private fun drainCommands() {
        val arr = runCatching { JSONArray(NativeBridge.nativeConsoleDrainCmds(handle)) }.getOrNull() ?: return
        for (i in 0 until arr.length()) {
            when (val c = arr.opt(i)) {
                is String -> when (c) {
                    "CancelWake" -> { wakeGen.incrementAndGet(); NativeBridge.nativeConsoleSetWake(handle, "null") }
                    "Probe" -> { resumeDiscovery(); pushHosts() }
                }
                is JSONObject -> {
                    c.optJSONObject("FetchLibrary")?.let { fetchLibrary(it, refreshOnly = false) }
                    c.optJSONObject("RefreshRunning")?.let { fetchLibrary(it, refreshOnly = true) }
                    c.optJSONObject("Pair")?.let(::pair)
                    c.optJSONObject("SendLogs")?.let(::sendLogs)
                    c.optJSONObject("SpeedTest")?.let(::speedTest)
                    c.optJSONObject("HostAction")?.let(::hostAction)
                    c.optJSONObject("SaveHost")?.let(::saveHost)
                    c.optJSONObject("UpdateHost")?.let(::updateHost)
                    c.optJSONObject("ForgetHost")?.let(::forgetHost)
                    c.optJSONObject("Wake")?.let(::wake)
                    c.optJSONObject("SetPin")?.let(::setPin)
                    c.optJSONObject("BindPreset")?.let(::bindPreset)
                    c.optJSONObject("SetClipboard")?.let(::setClipboard)
                    c.optJSONObject("OpenPlatformScreen")?.let { onPlatformScreen?.invoke(it.optString("id")) }
                    c.optJSONObject("PadAction")?.let { onPadAction?.invoke(it.optString("action"), it.optString("pad_key")) }
                    c.optString("OpenPlatformScreen").takeIf { c.has("OpenPlatformScreen") && c.opt("OpenPlatformScreen") is String }
                        ?.let { onPlatformScreen?.invoke(it) }
                }
            }
        }
    }

    private fun hostForKey(key: String): KnownHost? {
        val primary = key.substringBefore('\u0000')
        return knownHostStore.all().firstOrNull { ConsoleJson.rowKey(it.fpHex, it.address, it.port) == primary }
    }

    /**
     * A manual entry has no pin yet: it renames the placeholder at its address or adds one. A
     * record pinned there is another identity — the other OS of a dual-boot box — and keeps its name.
     */
    private fun saveHost(c: JSONObject) {
        val addr = c.optString("addr"); val port = c.optInt("port"); val name = c.optString("name")
        val existing = knownHostStore.placeholderAt(addr, port)
        if (existing != null) {
            if (name.isNotEmpty()) knownHostStore.save(existing.copy(name = name))
        } else {
            knownHostStore.save(KnownHost(address = addr, port = port, name = name.ifEmpty { addr }, fpHex = "", paired = false))
        }
        pushHosts(); pushKnownHosts()
    }

    private fun updateHost(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        val name = c.optString("name").trim(); val addr = c.optString("addr"); val port = c.optInt("port")
        if (addr != kh.address || port != kh.port) knownHostStore.remove(kh)
        knownHostStore.save(kh.copy(name = name.ifEmpty { addr }, address = addr, port = port))
        pushHosts(); pushKnownHosts()
    }

    private fun forgetHost(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        knownHostStore.remove(kh)
        appContext?.let { LibraryCache.standard(it.cacheDir).forget(kh.id) }
        pushHosts(); pushKnownHosts()
    }

    /**
     * `ConsoleCmd::BindPreset` — the host's default binding (`KnownHost.presetId`), or with
     * `game`, one title's ([KnownHost.gamePresets]). A null `preset_id` clears either.
     */
    private fun bindPreset(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        val pid = c.optString("preset_id")
            .takeIf { c.has("preset_id") && !c.isNull("preset_id") && it.isNotEmpty() }
        val game = c.optString("game")
            .takeIf { c.has("game") && !c.isNull("game") && it.isNotEmpty() }
        val next = when (game) {
            // Cleared bindings leave no key behind, so an unbound host stores an empty map.
            null -> kh.copy(presetId = pid)
            else -> kh.copy(
                gamePresets = kh.gamePresets.toMutableMap()
                    .apply { if (pid == null) remove(game) else put(game, pid) },
            )
        }
        knownHostStore.save(next)
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::SetClipboard` — the per-host clipboard trust toggle. */
    private fun setClipboard(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        knownHostStore.save(kh.copy(clipboardSync = c.optBoolean("on")))
        pushHosts(); pushKnownHosts()
    }

    private fun setPin(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        val pid = c.optString("preset_id"); val pin = c.optBoolean("pin")
        val pins = kh.pinnedPresetIds.toMutableList()
        if (pin && pid !in pins) pins.add(pid) else if (!pin) pins.remove(pid)
        knownHostStore.save(kh.copy(pinnedPresetIds = pins))
        pushHosts(); pushKnownHosts()
    }

    /**
     * `ConsoleCmd::SendLogs` — [io.unom.punktfunk.SendLogs], the same upload the touch home's
     * card menu runs; the result comes back here as a notice.
     */
    private fun sendLogs(c: JSONObject) {
        val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
        val hostName = c.optString("host_name").ifEmpty { addr }
        val id = identity
        if (id == null) {
            notice("Identity not ready yet — try again in a moment")
            return
        }
        val app = appContext ?: return
        ioPool.execute {
            val message = io.unom.punktfunk.SendLogs.toHost(app, id, addr, mgmt, fp, hostName)
            main.post { notice(message) }
        }
    }

    /**
     * `ConsoleCmd::SpeedTest` — [io.unom.punktfunk.runSpeedTest], the same measurement the touch
     * home's card menu runs. Only the PHASE crosses back: the console raised the takeover when
     * it sent the command, and owns clearing it.
     */
    private fun speedTest(c: JSONObject) {
        val key = c.optString("key")
        val addr = c.optString("addr"); val port = c.optInt("port"); val fp = c.optString("fp_hex")
        val id = identity
        if (id == null) {
            advanceSpeed(key, SpeedTestPhase.Failed("Identity not ready yet — try again in a moment"))
            return
        }
        val app = appContext ?: return
        // Same lane as the connect above: the probe blocks for its two-second burst, and the
        // console keeps drawing.
        ioPool.execute {
            kotlinx.coroutines.runBlocking {
                runSpeedTest(app, id, addr, port, fp) { p -> main.post { advanceSpeed(key, p) } }
            }
        }
    }

    /** The phase in `SpeedPhase`'s serde shape: unit variants are bare strings. */
    private fun advanceSpeed(key: String, p: SpeedTestPhase) {
        val json = when (p) {
            SpeedTestPhase.Connecting -> "\"Connecting\""
            SpeedTestPhase.Measuring -> "\"Measuring\""
            is SpeedTestPhase.Failed -> JSONObject().put("Failed", p.message).toString()
            is SpeedTestPhase.Done -> JSONObject().put(
                "Done",
                JSONObject()
                    .put("throughput_kbps", p.throughputKbps)
                    .put("loss_pct", p.lossPct)
                    .put("recommended_kbps", p.recommendedKbps),
            ).toString()
        }
        NativeBridge.nativeConsoleAdvanceSpeed(handle, key, json)
    }

    /**
     * Sleep / restart / shut the host down (`design/host-actions.md` §7) — the console already
     * confirmed a destructive one twice before raising this, and the host re-checks this
     * device's Host-power grant on arrival, so nothing is decided here.
     */
    private fun hostAction(c: JSONObject) {
        val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
        val hostName = c.optString("host_name").ifEmpty { addr }
        val actionId = c.optString("action_id"); val label = c.optString("label")
        val id = identity
        if (id == null) {
            notice("Identity not ready yet — try again in a moment")
            return
        }
        ioPool.execute {
            val message = HostActions.invoke(id, addr, mgmt, fp, hostName, actionId, label)
            main.post { notice(message) }
        }
    }

    private fun pair(c: JSONObject) {
        val addr = c.optString("addr"); val port = c.optInt("port")
        val pin = c.optString("pin"); val name = c.optString("device_name")
        val id = identity
        if (id == null) {
            NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairFailed("Identity not ready yet — try again in a moment"))
            return
        }
        NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairBusy())
        ioPool.execute {
            val fp = runCatching { NativeBridge.nativePair(addr, port, id.certPem, id.privateKeyPem, pin, name) }.getOrDefault("")
            main.post {
                if (fp.isNotEmpty()) {
                    // Named once the ceremony says who answered: the address may carry both OS
                    // installs of a dual-boot box, and the first record there is not this one.
                    val hostName = knownHostStore.resolve(fp, addr, port)?.name
                        ?: discovered.firstOrNull { it.fingerprint.equals(fp, true) }?.name
                        ?: discovered.firstOrNull { it.fingerprint == null && it.host == addr && it.port == port }?.name
                        ?: addr
                    knownHostStore.trust(addr, port, hostName, fp, paired = true)
                    pushHosts(); pushKnownHosts()
                    NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairPaired(fp))
                } else {
                    NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairFailed(ConnectErrors.pairMessage(NativeBridge.nativeTakeLastError())))
                }
            }
        }
    }

    /**
     * The wake-and-wait loop (the desktop's `spawn_wake`): resend the magic packet every 6 s,
     * probe once a second, 90 s timeout; the console reads `online`/`timed_out` off the status
     * and acts (a `then_connect` wake dials from the shell's side once online).
     */
    private fun wake(c: JSONObject) {
        val key = c.optString("key"); val thenConnect = c.optBoolean("then_connect")
        val kh = hostForKey(key) ?: return
        if (kh.mac.isEmpty()) return
        val gen = wakeGen.incrementAndGet()
        val name = kh.name.ifBlank { kh.address }
        ioPool.execute {
            val started = System.currentTimeMillis()
            var lastPacket = 0L
            while (wakeGen.get() == gen && handle != 0L) {
                val elapsed = ((System.currentTimeMillis() - started) / 1000).toInt()
                val timedOut = elapsed >= 90
                if (!timedOut && System.currentTimeMillis() - lastPacket >= 6_000) {
                    NativeBridge.nativeWakeOnLan(kh.mac.joinToString(","), kh.address)
                    lastPacket = System.currentTimeMillis()
                }
                val online = Presence.isSelf(kh, NativeBridge.nativeProbe(kh.address, kh.port, 900)) ||
                    discovered.any { kh.matches(it) }
                if (wakeGen.get() != gen) return@execute
                NativeBridge.nativeConsoleSetWake(
                    handle,
                    ConsoleJson.wakeStatus(key, name, elapsed, timedOut, online, thenConnect),
                )
                if (online || timedOut) return@execute
                Thread.sleep(1000)
            }
        }
    }

    /**
     * The library pipeline (the desktop's `spawn_fetch`): cached shelf first, wake + retry
     * across the boot window when the host has a MAC, then the catalog, the running set and
     * the posters — each poster fetched over the same mTLS client and pushed as bytes.
     */
    private fun fetchLibrary(c: JSONObject, refreshOnly: Boolean) {
        val app = appContext ?: return
        val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
        val id = identity
        val kh = knownHostStore.getByFp(fp)
        if (refreshOnly) {
            if (id == null) return
            ioPool.execute {
                val games = LibraryClient.fetchRunning(addr, mgmt, id.certPem, id.privateKeyPem, fp)
                main.post {
                    if (handle == 0L) return@post
                    NativeBridge.nativeConsoleLibraryRunning(handle, ConsoleJson.runningGames(games))
                    // The carousel behind the shelf shows the same fact from its own map; this
                    // answer is fresher than anything its TTL would fetch.
                    nowPlayingAt[fp] = android.os.SystemClock.elapsedRealtime()
                    recordNowPlaying(fp, games)
                }
            }
            return
        }
        val gen = fetchGen.incrementAndGet()
        NativeBridge.nativeConsoleLibraryBegin(handle)
        if (id == null) {
            NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError("Couldn't load the library", "Identity not ready yet — try again in a moment", true))
            return
        }
        val cache = LibraryCache.standard(app.cacheDir)
        val cacheKey = kh?.id ?: fp.ifEmpty { "$addr:$mgmt" }
        ioPool.execute {
            val cached = cache.load(cacheKey)?.games?.takeIf { it.isNotEmpty() }
            if (cached != null) main.post { if (gen == fetchGen.get()) NativeBridge.nativeConsoleLibraryGames(handle, ConsoleJson.libraryGames(cached), true) }
            val macs = kh?.mac.orEmpty()
            val waking = macs.isNotEmpty() && settings.autoWakeEnabled
            if (waking) NativeBridge.nativeWakeOnLan(macs.joinToString(","), addr)
            val attempts = if (waking) 12 else 1
            var result: LibraryResult? = null
            for (attempt in 0 until attempts) {
                if (gen != fetchGen.get()) return@execute
                val r = LibraryClient.fetch(addr, mgmt, id.certPem, id.privateKeyPem, fp)
                result = r
                if (r is LibraryResult.Ok || r is LibraryResult.Unauthorized) break
                if (attempt + 1 >= attempts) break
                if (attempt % 2 == 1) NativeBridge.nativeWakeOnLan(macs.joinToString(","), addr)
                main.post { if (gen == fetchGen.get()) NativeBridge.nativeConsoleLibraryStale(handle, 1) }
                Thread.sleep(5_000)
            }
            if (gen != fetchGen.get()) return@execute
            when (val r = result) {
                is LibraryResult.Ok -> {
                    val games = r.games
                    cache.store(cacheKey, games)
                    val up = LibraryClient.fetchRunning(addr, mgmt, id.certPem, id.privateKeyPem, fp)
                    main.post {
                        if (gen != fetchGen.get()) return@post
                        NativeBridge.nativeConsoleLibraryGames(handle, ConsoleJson.libraryGames(games), false)
                        NativeBridge.nativeConsoleLibraryStale(handle, 0)
                        NativeBridge.nativeConsoleLibraryRunning(handle, ConsoleJson.runningGames(up))
                    }
                    pumpArt(games, gen, id, addr, fp, offline = false)
                }
                is LibraryResult.Unauthorized -> {
                    if (cached != null) pumpArt(cached, gen, id, addr, fp, offline = true)
                    main.post {
                        if (gen != fetchGen.get()) return@post
                        if (cached != null) NativeBridge.nativeConsoleLibraryStale(handle, 2)
                        else NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError("Not paired", r.message, false))
                    }
                }
                is LibraryResult.Error -> {
                    if (cached != null) pumpArt(cached, gen, id, addr, fp, offline = true)
                    main.post {
                        if (gen != fetchGen.get()) return@post
                        if (cached != null) NativeBridge.nativeConsoleLibraryStale(handle, 2)
                        else NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError("Couldn't load the library", r.message, true))
                    }
                }
                null -> {}
            }
        }
    }

    /**
     * Every poster on this shelf, one job each.
     *
     * Also runs when the host did not answer: the shelf is drawn from the library cache and
     * the covers for it are on disk too, so a lettered placeholder next to "last known
     * library" is a picture thrown away rather than one we never had.
     */
    private fun pumpArt(
        games: List<GameEntry>,
        gen: Long,
        id: ClientIdentity,
        addr: String,
        fp: String,
        offline: Boolean,
    ) {
        for (g in games) {
            val candidates = g.art.posterCandidates
            if (candidates.isEmpty()) continue
            artPool.execute {
                if (gen != fetchGen.get()) return@execute
                val bytes = fetchArt(candidates, id, addr, fp, offline) ?: return@execute
                main.post { if (gen == fetchGen.get() && handle != 0L) NativeBridge.nativeConsoleLibraryArt(handle, g.id, bytes) }
            }
        }
    }

    /** One poster: the candidates in order, first success wins; the host's art proxy over mTLS. */
    private fun fetchArt(candidates: List<String>, id: ClientIdentity, addr: String, fp: String, offline: Boolean): ByteArray? {
        for (url in candidates) {
            val client = if (url.contains(addr)) {
                runCatching { io.unom.punktfunk.kit.library.mtlsHttpClient(id.certPem, id.privateKeyPem, addr, fp, artCache) }.getOrNull() ?: continue
            } else artHttp
            val req = Request.Builder().url(url)
            // With the host down the cache is the only answer there is. Left to itself OkHttp
            // honours the proxy's `max-age`, goes to revalidate once it lapses, fails to
            // connect, and reports a miss on bytes that are sitting on disk.
            if (offline) req.cacheControl(CacheControl.FORCE_CACHE)
            val bytes = runCatching {
                client.newCall(req.build()).execute().use { resp ->
                    if (resp.code == 200) resp.body?.bytes()?.takeIf { it.isNotEmpty() && it.size <= 16 shl 20 } else null
                }
            }.getOrNull()
            if (bytes != null) return bytes
        }
        return null
    }

    /** The no-PIN request-access park (≥ the host's approval window) — ConnectScreen's figure. */
    private const val REQUEST_ACCESS_TIMEOUT_MS = 185_000

    /** How long a host's advertised actions stay fresh before we ask again — the desktop's
     *  `pf_client_core::host_actions::TTL`. Long on purpose: what it governs changes when an
     *  operator edits access, not minute to minute, and each refresh is a TLS handshake. */
    private const val HOST_ACTIONS_TTL_MS = 300_000L

    /** The presence cadence — the desktop's. */
    private const val SWEEP_MS = 12_000L

    /** How long a host's running title stays fresh — `pf_client_core::library::RUNNING_TTL`.
     *  Short: this is the one host fact that changes while somebody is looking at the tile. */
    private const val NOW_PLAYING_TTL_MS = 20_000L
}
