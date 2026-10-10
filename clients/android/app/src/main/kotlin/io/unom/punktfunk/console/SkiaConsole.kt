package io.unom.punktfunk.console

import android.app.ActivityManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import io.unom.punktfunk.CONNECT_TIMEOUT_MS
import io.unom.punktfunk.ConnectErrors
import io.unom.punktfunk.HostActions
import io.unom.punktfunk.HostProfiles
import io.unom.punktfunk.HostRecords
import io.unom.punktfunk.ProfilesAnswer
import io.unom.punktfunk.PresetStore
import io.unom.punktfunk.REQUEST_ACCESS_TIMEOUT_MS
import io.unom.punktfunk.SessionFactory
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
import io.unom.punktfunk.kit.ProfilePick
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.kit.deviceBodyVibrator
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.discovery.Presence
import io.unom.punktfunk.kit.discovery.PresenceTracker
import io.unom.punktfunk.kit.discovery.WakeLoop
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.link.StartScreen
import io.unom.punktfunk.kit.link.host
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.RunningGame
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityHolder
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.security.nowSecs
import io.unom.punktfunk.models.ActiveSession
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import org.json.JSONArray
import org.json.JSONObject

/**
 * The Skia console (`crates/client/pf-console-ui`, drawn by native over EGL — design
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

    internal var handle = 0L
        private set

    /**
     * False once the console has proven it cannot draw — the native create failed, or the render
     * thread died (a GL context that never came up, or one Android reclaimed and that would not
     * come back). Compose observes it: `App` folds it into the gamepad-UI gate, so the answer to a
     * dead console is the touch UI — not the gray, never-painted `SurfaceView` the shell would
     * otherwise sit on for the rest of the process.
     */
    var healthy by mutableStateOf(true)
        private set

    internal var appContext: Context? = null
        private set
    internal val main = Handler(Looper.getMainLooper())
    internal val ioPool = Executors.newCachedThreadPool { r -> Thread(r, "pf-console-io").apply { isDaemon = true } }
    /** One thread, so pad lists reach the console in the order they were asked for. */
    private val padPool = Executors.newSingleThreadExecutor { r -> Thread(r, "pf-console-pads").apply { isDaemon = true } }
    private var eventThread: Thread? = null
    private val running = AtomicBoolean(false)

    // Services.
    internal lateinit var knownHostStore: KnownHostStore
        private set
    private lateinit var presetStore: PresetStore
    private lateinit var settingsStore: SettingsStore
    internal lateinit var identities: IdentityHolder
        private set
    internal val identity: ClientIdentity? get() = identities.current
    /** A link that arrived before the first identity load ended; replayed once it has. Main-thread only. */
    private var parkedLink: String? = null
    private var discovery: HostDiscovery? = null
    private var discovered: List<DiscoveredHost> = emptyList()

    /** Record ids that answered the probe — the whole of presence. See [sweep]. */
    private var reachable: Set<String> = emptySet()
    private val presence = PresenceTracker()
    internal var settings: Settings = Settings()
        private set

    /** What each paired host last said this device may do TO it, by fingerprint, and when we
     *  last asked — the Android half of the desktop's shared actions cache. Main-thread only. */
    private val hostActions = mutableMapOf<String, List<HostActions.Action>>()
    private val hostActionsAt = mutableMapOf<String, Long>()

    /** What each paired host has UP, by fingerprint, and when we last asked — the same shape
     *  on a much shorter fuse (`pf_client_core::library::RUNNING_TTL`). Main-thread only. */
    private val nowPlaying = mutableMapOf<String, String>()
    internal val nowPlayingAt = mutableMapOf<String, Long>()

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
    private var onPadAction: ((String, String) -> Unit)? = null
    private var onPulse: ((String) -> Unit)? = null

    /** The console's focus, in words, whenever it changes — the shell speaks it to TalkBack. */
    private var onAnnounce: ((String) -> Unit)? = null

    /** The connect in flight, if any — cancelable through `OverlayAction::CancelConnect`. */
    private class Dial(val cancelled: AtomicBoolean = AtomicBoolean(false))
    private var dial: Dial? = null

    /** The wake-and-wait loop in flight, if any. */
    private var wakeGen = AtomicLong(0)
    private val pairGen = AtomicLong(0)

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
        identities = IdentityHolder.shared(app)
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
            // No clipboard worth copying a link to on a TV.
            .put("tv", io.unom.punktfunk.isTvDevice(app))
            // The same MediaCodec answer the Hello advertises by: without a real AV1
            // decoder the codec row marks AV1 unsupported instead of offering a dead pick.
            .put("av1_ok", VideoDecoders.decodableCodecBits() and 4 != 0)
            // Answers `FetchProfiles`, so a connect checks the box's profiles first.
            .put("profiles", true)
            .put("settings", ConsoleJson.settings(initial, base))
            .put("presets", JSONArray(ConsoleJson.presets(presets)))
            .put("known_hosts", JSONObject(ConsoleJson.knownHosts(knownHostStore.all())))
            .put("entry", startEntry(initial, pendingLink, presets))
        // A phone's own shape leads the Aspect row; a TV's panel is a standard one.
        if (!io.unom.punktfunk.isTvDevice(app)) {
            val (panel, safe) = io.unom.punktfunk.panelScreens(app)
            opts.put("screen", JSONArray(panel.toList())).put("safe_area", JSONArray(safe.toList()))
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
     * Only while the console is ON SCREEN (attached, the app in front): parked behind the touch
     * UI, a stream or Home there is nobody to show the pips to — and mid-stream the radio belongs
     * to the session. The timer keeps ticking so probes resume within a cadence of coming back.
     */
    private val sweep = object : Runnable {
        override fun run() {
            if (handle == 0L) return
            main.removeCallbacks(this)
            main.postDelayed(this, SWEEP_MS)
            if (onConnected == null || discovery?.appVisible == false) return
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
        identities.ensure()
        discovery = HostDiscovery.shared(app).also { it.addNetworkListener(onNetworkChanged) }
        resumeDiscovery()
        // Commands from the console, drained on a short cadence once the identity load ends:
        // a start entry queues its shelf fetch or desktop dial before that, and a cold-start
        // link waits in `parkedLink`.
        main.post(object : Runnable {
            override fun run() {
                if (handle == 0L) return
                if (identities.settled) {
                    parkedLink?.let { parkedLink = null; handleDeepLink(it) }
                    drainCommands()
                }
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
        onPadAction: (String, String) -> Unit,
        onPulse: (String) -> Unit,
        onAnnounce: (String) -> Unit,
    ) {
        this.onConnected = onConnected
        this.onSettingsChange = onSettingsChange
        this.onQuit = onQuit
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
     * A cold start delivers the link before the identity load ends; it waits for that.
     */
    fun handleDeepLink(url: String) {
        if (handle == 0L) return
        if (!identities.settled) {
            parkedLink = url
            return
        }
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

    /**
     * The connected controllers, for the chip + settings rows, plus pads with no `InputDevice`.
     * Built on [padPool]: each pad's battery and motor reads are binder calls into the input
     * service, which can stall for seconds and must never hold the main thread.
     */
    internal fun padsChanged(driving: InputDevice?, extras: List<ConsoleJson.ExtraPad> = emptyList()) {
        val h = handle
        if (h == 0L) return
        val app = appContext
        padPool.execute {
            NativeBridge.nativeConsoleSetPads(
                h,
                ConsoleJson.pads(
                    Gamepad.pads(),
                    driving ?: Gamepad.firstPad(),
                    extras,
                    app?.let(::deviceBodyVibrator),
                    ConsoleJson.otherInputs(),
                ),
            )
        }
    }

    /**
     * The driving pad's reading while the console's input test is on (`ConsoleCmd::PadTest`),
     * null otherwise. While set, the shell's probes feed it instead of the menu. Main-thread only.
     */
    internal var padTest: PadTestReading? = null
        private set

    internal fun pushPadTest() {
        val t = padTest ?: return
        if (handle != 0L) NativeBridge.nativeConsoleSetPadTest(handle, t.json())
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
    internal fun recordNowPlaying(fpHex: String, games: List<RunningGame>) {
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
        if (next != settings) adoptSettings(next)
    }

    /** Store [next] and hand it to the app, whose [settingsChanged] pushes it back to the console. */
    private fun adoptSettings(next: Settings) {
        settings = next
        settingsStore.save(next)
        onSettingsChange?.invoke(next)
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
        val profile = a.optString("profile").takeIf { a.has("profile") && !a.isNull("profile") && it.isNotEmpty() }
        val requestAccess = a.optBoolean("request_access", false)
        val id = identity
        if (id == null) {
            NativeBridge.nativeConsoleSessionPhase(handle, 2, identities.blockedMessage())
            return
        }
        // The shell raises its hold for a GAME launch off a shelf; a desktop connect and a
        // launcher tile go straight through, so the session is handed over at once. Mirrors
        // `Shell::launch_hold`, and reads the same cached catalog the shelf was drawn from.
        holdsLaunch = launchId != null &&
            LibraryCache.standard(app.cacheDir).load(LibraryCache.keyFor(kh, fp))?.games
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
                    preset = preset,
                    profile = profile,
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
                        val name = record?.name
                            ?: discovered.firstOrNull { it.host == addr && it.port == port }?.name
                            ?: addr
                        val paired = requestAccess || record?.paired == true
                        SessionFactory.pinPresented(h, addr, port, name, paired, knownHostStore)?.let {
                            record = it
                            pushHosts(); pushKnownHosts()
                        }
                    }
                    val session = SessionFactory.afterDial(h, record, effective, preset, knownHostStore)
                        .copy(launchedFromLibrary = launchId != null, libraryPresetId = presetId)
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
                    if (token == "profile-unknown" && kh != null) {
                        HostRecords.savePick(knownHostStore, kh, null)
                        pushHosts(); pushKnownHosts()
                    }
                    // 5: the console forgets the pick and asks the box's list once more.
                    NativeBridge.nativeConsoleSessionPhase(
                        handle, if (token == "profile-unknown") 5 else 2,
                        ConnectErrors.connectMessage(token, requestAccess),
                    )
                    resumeDiscovery()
                }
            }
        }
    }

    // ---- commands from the console -----------------------------------------------------

    /** `ConsoleCmd::LoadLicenses`: the notices this APK bundles, for the console's Licences screen. */
    private fun loadLicenses() {
        val app = appContext ?: return
        ioPool.execute {
            val notices = runCatching {
                app.assets.open("THIRD-PARTY-NOTICES.txt").bufferedReader().use { it.readText() }
            }.getOrDefault("Third-party notices unavailable.")
            val json = JSONArray()
                .put(JSONObject().put("heading", "Third-party software").put("text", notices))
                .toString()
            main.post { if (handle != 0L) NativeBridge.nativeConsoleSetLicenses(handle, json) }
        }
    }

    /** Runs what the console queued. A unit variant is a bare string, one with fields `{"Name": {…}}`. */
    private fun drainCommands() {
        val arr = runCatching { JSONArray(NativeBridge.nativeConsoleDrainCmds(handle)) }.getOrNull() ?: return
        for (i in 0 until arr.length()) {
            when (val c = arr.opt(i)) {
                is String -> commands[c]?.invoke(JSONObject())
                is JSONObject -> for (name in c.keys()) c.optJSONObject(name)?.let { commands[name]?.invoke(it) }
            }
        }
    }

    /**
     * Every `ConsoleCmd` this shell runs, by serde name. `clients/shared/console-bridge-vectors.json`
     * holds one of each; a name missing here must be in [ignoredCommands].
     */
    internal val commands: Map<String, (JSONObject) -> Unit> = mapOf(
        "CancelWake" to { _ -> wakeGen.incrementAndGet(); NativeBridge.nativeConsoleSetWake(handle, "null") },
        "Probe" to { _ -> resumeDiscovery(); pushHosts() },
        "LoadLicenses" to { _ -> loadLicenses() },
        "FetchLibrary" to { c -> fetchLibrary(c, refreshOnly = false) },
        "RefreshRunning" to { c -> fetchLibrary(c, refreshOnly = true) },
        "Pair" to ::pair,
        "RequestAccess" to ::requestAccess,
        "SendLogs" to ::sendLogs,
        "SpeedTest" to ::speedTest,
        "HostAction" to ::hostAction,
        "EndGame" to this::endGame,
        "Install" to this::changeInstall,
        "SaveHost" to ::saveHost,
        "UpdateHost" to ::updateHost,
        "ForgetHost" to ::forgetHost,
        "UnpairHost" to ::unpairHost,
        "SavePreset" to ::savePreset,
        "DeletePreset" to { c -> presetStore.delete(c.optString("id")); pushPresets() },
        "Wake" to ::wake,
        "SetPin" to ::setPin,
        "FetchProfiles" to ::fetchProfiles,
        "WakeProfile" to ::wakeProfile,
        "SetProfile" to ::setProfile,
        "BindPreset" to ::bindPreset,
        "SetClipboard" to ::setClipboard,
        "PadTest" to { c -> padTest = if (c.optBoolean("on")) PadTestReading() else null },
        "PadAction" to { c -> onPadAction?.invoke(c.optString("action"), c.optString("pad_key")) },
    )

    /** `ConsoleCmd`s this shell drops: it raises no prompt, so no `PromptAnswer` comes back. */
    internal val ignoredCommands = setOf("PromptAnswer")

    private fun hostForKey(key: String): KnownHost? {
        val primary = ConsoleJson.hostKey(key)
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
            knownHostStore.save(
                KnownHost(
                    address = addr, port = port, name = name.ifEmpty { addr }, fpHex = "", paired = false,
                    addedAt = nowSecs(),
                ),
            )
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

    /**
     * `ConsoleCmd::ForgetHost` — [HostRecords.forget]. A cleared default host is saved the way
     * the console's own settings edits are.
     */
    private fun forgetHost(c: JSONObject) {
        val app = appContext ?: return
        val kh = hostForKey(c.optString("key")) ?: return
        HostRecords.forget(app, knownHostStore, kh, settings)?.let(::adoptSettings)
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::SavePreset`: the console saved one preset whole, merged onto the stored one. */
    private fun savePreset(c: JSONObject) {
        val id = c.optString("id").takeIf { it.isNotEmpty() } ?: return
        val stored = presetStore.byId(id) ?: StreamPreset(id = id, name = "")
        val overrides = io.unom.punktfunk.SettingsOverlay.fromConsoleJson(
            c.optJSONObject("overrides") ?: JSONObject(), stored.overrides,
        )
        presetStore.save(stored.copy(name = c.optString("name"), overrides = overrides))
        pushPresets()
    }

    private fun pushPresets() {
        if (handle != 0L) NativeBridge.nativeConsoleSetPresets(handle, ConsoleJson.presets(presetStore.all()))
    }

    /** `ConsoleCmd::UnpairHost` — [HostRecords.unpair]. */
    private fun unpairHost(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        HostRecords.unpair(knownHostStore, kh)
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::BindPreset` — [HostRecords.bindPreset]; a null `preset_id` clears. */
    private fun bindPreset(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        val pid = c.optString("preset_id")
            .takeIf { c.has("preset_id") && !c.isNull("preset_id") && it.isNotEmpty() }
        val game = c.optString("game")
            .takeIf { c.has("game") && !c.isNull("game") && it.isNotEmpty() }
        HostRecords.bindPreset(knownHostStore, kh, pid, game)
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::SetClipboard` — [HostRecords.setClipboard]. */
    private fun setClipboard(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        HostRecords.setClipboard(knownHostStore, kh, c.optBoolean("on"))
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::FetchProfiles`: ask the host who plays on it; the answer goes back keyed on its pin. */
    private fun fetchProfiles(c: JSONObject) {
        val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
        val id = identity
        ioPool.execute {
            val answer = if (id == null) {
                ProfilesAnswer.Failed(identities.blockedMessage())
            } else {
                HostProfiles.fetch(id, addr, mgmt, fp)
            }
            val json = ConsoleJson.profilesAnswer(answer)
            main.post { if (handle != 0L) NativeBridge.nativeConsoleSetProfiles(handle, fp, json) }
        }
    }

    /** `ConsoleCmd::WakeProfile`: start a stopped seat. No answer: the shell polls `FetchProfiles`. */
    private fun wakeProfile(c: JSONObject) {
        val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
        val profile = c.optString("id")
        val id = identity ?: return
        ioPool.execute { HostProfiles.wake(id, addr, mgmt, fp, profile) }
    }

    /** `ConsoleCmd::SetProfile`: save (or with no profile, clear) the pick on a host. */
    private fun setProfile(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        val p = c.optJSONObject("profile")
        HostRecords.savePick(knownHostStore, kh, p?.let { ProfilePick(it.optString("id"), it.optString("display_name")) })
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::SetPin` — [HostRecords.setPin]. */
    private fun setPin(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        HostRecords.setPin(knownHostStore, kh, c.optString("preset_id"), c.optBoolean("pin"))
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
            notice(identities.blockedMessage())
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
            advanceSpeed(key, SpeedTestPhase.Failed(identities.blockedMessage()))
            return
        }
        val app = appContext ?: return
        // Same lane as the connect above: the probe blocks for its two-second burst, and the
        // console keeps drawing.
        ioPool.execute {
            kotlinx.coroutines.runBlocking {
                runSpeedTest(
                    app, id, addr, port, fp,
                    onProgress = { kbps -> main.post { advanceSpeedProgress(key, kbps) } },
                ) { p -> main.post { advanceSpeed(key, p) } }
            }
        }
    }

    private fun advanceSpeed(key: String, p: SpeedTestPhase) {
        NativeBridge.nativeConsoleAdvanceSpeed(handle, key, ConsoleJson.speedPhase(p))
    }

    /** A mid-burst figure for the console's graph. */
    private fun advanceSpeedProgress(key: String, kbps: Int) {
        NativeBridge.nativeConsoleAdvanceSpeed(handle, key, ConsoleJson.speedProgress(kbps))
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
            notice(identities.blockedMessage())
            return
        }
        ioPool.execute {
            val message = HostActions.invoke(id, addr, mgmt, fp, hostName, actionId, label)
            main.post { notice(message) }
        }
    }

    private fun pair(c: JSONObject) {
        val pin = c.optString("pin"); val name = c.optString("device_name")
        runPairing(c, ConnectErrors::pairMessage) { id, addr, port ->
            NativeBridge.nativePair(addr, port, id.certPem, id.privateKeyPem, pin, name)
        }
    }

    /** No PIN: wait for the host's operator, pinned to the advertised fingerprint. Never streams. */
    private fun requestAccess(c: JSONObject) {
        val fpHex = c.optString("fp_hex"); val name = c.optString("device_name")
        runPairing(c, { ConnectErrors.connectMessage(it, requestAccess = true) }) { id, addr, port ->
            NativeBridge.nativeRequestAccess(addr, port, id.certPem, id.privateKeyPem, fpHex, name)
        }
    }

    /**
     * One pairing off the main thread ([ask] returns the host fingerprint, or `""`), reported Busy,
     * then Paired or Failed. A later pairing makes this one's answer stale.
     */
    private fun runPairing(
        c: JSONObject,
        wording: (String) -> String,
        ask: (ClientIdentity, String, Int) -> String,
    ) {
        val addr = c.optString("addr"); val port = c.optInt("port")
        val id = identity
        if (id == null) {
            NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairFailed(identities.blockedMessage()))
            return
        }
        val gen = pairGen.incrementAndGet()
        NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairBusy())
        ioPool.execute {
            val fp = runCatching { ask(id, addr, port) }.getOrDefault("")
            val err = if (fp.isEmpty()) NativeBridge.nativeTakeLastError() else ""
            main.post {
                if (pairGen.get() != gen) return@post
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
                    NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairFailed(wording(err)))
                }
            }
        }
    }

    /**
     * The wake-and-wait loop ([WakeLoop], the desktop's `spawn_wake`); the console reads
     * `online`/`timed_out` off the status and acts (a `then_connect` wake dials from the shell's
     * side once online). Online is a probe of the host, at its live advert's address if it has one.
     */
    private fun wake(c: JSONObject) {
        val key = c.optString("key"); val thenConnect = c.optBoolean("then_connect")
        val kh = hostForKey(key) ?: return
        if (kh.mac.isEmpty()) return
        val gen = wakeGen.incrementAndGet()
        val name = kh.name.ifBlank { kh.address }
        ioPool.execute {
            WakeLoop.run(
                kh.mac, kh.address,
                isOnline = {
                    Presence.probeSelf(kh, discovered.firstOrNull { kh.matches(it) }) { addr, port ->
                        NativeBridge.nativeProbe(addr, port, 900)
                    }
                },
                cancelled = { wakeGen.get() != gen || handle == 0L },
            ) { seconds, timedOut, online ->
                NativeBridge.nativeConsoleSetWake(
                    handle,
                    ConsoleJson.wakeStatus(key, name, seconds, timedOut, online, thenConnect),
                )
            }
        }
    }

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

/** One pad's held buttons and axes, by the names the console's `PadTestState` reads. */
internal class PadTestReading {
    private val keys = linkedSetOf<String>()
    /** The d-pad as a HAT, which many pads report instead of keys. */
    private val hat = linkedSetOf<String>()
    private var axes: Map<String, Float> = emptyMap()

    /** A key by its CORRECTED code ([Gamepad.padKeyCode]), as the stream reads it. */
    fun key(code: Int, down: Boolean) {
        val name = TEST_NAMES[code] ?: return
        if (down) keys += name else keys -= name
    }

    fun motion(ev: MotionEvent) {
        axes = io.unom.punktfunk.padAxes(ev)
        val (hx, hy) = (axes["HX"] ?: 0f) to (axes["HY"] ?: 0f)
        hat.clear()
        if (hx < -0.5f) hat += "Left"
        if (hx > 0.5f) hat += "Right"
        if (hy < -0.5f) hat += "Up"
        if (hy > 0.5f) hat += "Down"
    }

    fun json(): String {
        val held = JSONArray()
        (keys + hat).forEach { held.put(it) }
        val ax = JSONArray()
        for (n in listOf("LX", "LY", "RX", "RY", "LT", "RT")) {
            ax.put(JSONArray().put(n).put((axes[n] ?: 0f).toDouble()))
        }
        return JSONObject().put("held", held).put("axes", ax).toString()
    }

    private companion object {
        val TEST_NAMES = mapOf(
            KeyEvent.KEYCODE_BUTTON_A to "A",
            KeyEvent.KEYCODE_BUTTON_B to "B",
            KeyEvent.KEYCODE_BUTTON_X to "X",
            KeyEvent.KEYCODE_BUTTON_Y to "Y",
            KeyEvent.KEYCODE_BUTTON_L1 to "LB",
            KeyEvent.KEYCODE_BUTTON_R1 to "RB",
            KeyEvent.KEYCODE_BUTTON_L2 to "LT",
            KeyEvent.KEYCODE_BUTTON_R2 to "RT",
            KeyEvent.KEYCODE_BUTTON_SELECT to "Back",
            KeyEvent.KEYCODE_BUTTON_START to "Start",
            KeyEvent.KEYCODE_BUTTON_MODE to "Guide",
            KeyEvent.KEYCODE_BUTTON_THUMBL to "LS",
            KeyEvent.KEYCODE_BUTTON_THUMBR to "RS",
            KeyEvent.KEYCODE_DPAD_UP to "Up",
            KeyEvent.KEYCODE_DPAD_DOWN to "Down",
            KeyEvent.KEYCODE_DPAD_LEFT to "Left",
            KeyEvent.KEYCODE_DPAD_RIGHT to "Right",
        )
    }
}
