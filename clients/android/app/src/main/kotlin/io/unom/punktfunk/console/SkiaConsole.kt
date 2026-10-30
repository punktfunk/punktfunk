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
import io.unom.punktfunk.DialSpec
import io.unom.punktfunk.HostActions
import io.unom.punktfunk.HostProfiles
import io.unom.punktfunk.HostRecords
import io.unom.punktfunk.HostServices
import io.unom.punktfunk.JniHostNative
import io.unom.punktfunk.ProfilesAnswer
import io.unom.punktfunk.PENDING_POLL_MS
import io.unom.punktfunk.PresetStore
import io.unom.punktfunk.REQUEST_ACCESS_TIMEOUT_MS
import io.unom.punktfunk.Settings
import io.unom.punktfunk.SettingsStore
import io.unom.punktfunk.SpeedTestPhase
import io.unom.punktfunk.StreamPreset
import io.unom.punktfunk.deviceName
import io.unom.punktfunk.effectiveFor
import io.unom.punktfunk.matches
import io.unom.punktfunk.pinTitleShortcut
import io.unom.punktfunk.runSpeedTest
import io.unom.punktfunk.shortcutId
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.ProfilePick
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.kit.deviceBodyVibrator
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.HostResolution
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
 * This object is the console's adapter over [HostServices] (`ConsoleShared` / `LibraryShared` /
 * `ConsoleBus` on the native side): it turns the console's commands and asks (`OverlayAction`)
 * into a dial, a pairing, a wake, the library pipeline, the settings round-trip, a clipboard
 * write or a task-to-back, and their results into bridge JSON. Host facts live in [services];
 * what the console itself changes lives in [state], which only the main thread reaches.
 * Everything blocking runs on its own executor; every native call is cheap.
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

    /** Written once by [ensure] before the event thread starts; `0` until then. */
    @Volatile
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

    /** The two cadences: the presence sweep and the command drain. */
    private val main = Handler(Looper.getMainLooper())
    internal val ioPool = Executors.newCachedThreadPool { r -> Thread(r, "pf-console-io").apply { isDaemon = true } }
    /** One thread, so pad lists reach the console in the order they were asked for. */
    private val padPool = Executors.newSingleThreadExecutor { r -> Thread(r, "pf-console-pads").apply { isDaemon = true } }
    private var eventThread: Thread? = null
    private val running = AtomicBoolean(false)

    // Services.
    /** Dial, pin, wake and presence: every host fact the console shows comes from here. */
    internal lateinit var services: HostServices
        private set
    private lateinit var presetStore: PresetStore
    private lateinit var settingsStore: SettingsStore
    internal lateinit var identities: IdentityHolder
        private set
    internal val identity: ClientIdentity? get() = identities.current
    private var discovery: HostDiscovery? = null

    /** Everything the console changes after [ensure]; the main thread reaches it, nothing else. */
    internal val state = MainConfined(ConsoleState())

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
        services = HostServices(KnownHostStore(app), JniHostNative(app))
        identities = IdentityHolder.shared(app)
        presetStore = PresetStore(app)
        settingsStore = SettingsStore(app)
        state.now { settings = initial }
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
            .put("known_hosts", JSONObject(ConsoleJson.knownHosts(services.store.all())))
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
        val hosts = services.store.all()
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
        services.discovered = list
        // Learn names / wake MACs / mgmt ports from live adverts, as the desktop service does.
        ioPool.execute {
            var changed = false
            for (dh in list) {
                val kh = services.store.all().firstOrNull { it.matches(dh) } ?: continue
                if (services.store.learnName(kh, dh.name)) changed = true
                if (dh.mac.isNotEmpty() && dh.mac.toSet() != kh.mac.toSet()) {
                    services.store.learnMac(kh, dh.mac); changed = true
                }
                dh.mgmtPort?.let { if (it != kh.mgmtPort) { services.store.learnMgmtPort(kh, it); changed = true } }
                if (dh.os.isNotEmpty() && dh.os != kh.os) { services.store.learnOs(kh, dh.os); changed = true }
            }
            state.launch { pushHosts(); if (changed) pushKnownHosts() }
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
            if (state.now { onConnected == null } || discovery?.appVisible == false) return
            val saved = services.store.all()
            val live = services.discovered
            ioPool.execute {
                // A pinned host that answered somewhere else has moved, and the sweep followed it
                // so the dial and the library fetch go where it lives.
                val swept = services.sweep(saved) { kh -> live.firstOrNull { kh.matches(it) } }
                state.launch {
                    if (swept.changed || swept.moved) {
                        pushHosts()
                        if (swept.moved) pushKnownHosts()
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
                    state.now { parkedLink.also { parkedLink = null } }?.let(::handleDeepLink)
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
                state.launch { onEvent(ev) }
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
        onSaveLauncherFile: (String, String) -> Unit,
    ) {
        state.now {
            this.onConnected = onConnected
            this.onSettingsChange = onSettingsChange
            this.onQuit = onQuit
            this.onPadAction = onPadAction
            this.onPulse = onPulse
            this.onAnnounce = onAnnounce
            this.onSaveLauncherFile = onSaveLauncherFile
        }
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
        state.now {
            onConnected = null
            onSettingsChange = null
            onQuit = null
            onPadAction = null
            onPulse = null
            onAnnounce = null
            onSaveLauncherFile = null
        }
    }

    /** The touch UI (or a link) changed settings: the console reads the new snapshot next. */
    fun settingsChanged(s: Settings) {
        state.now { settings = s }
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
        val kh = services.store.byId(hostId) ?: return
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
            state.now { parkedLink = url }
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
        when (val resolved = io.unom.punktfunk.kit.link.DeepLinks.resolveHost(link, services.store.all())) {
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
                state.now {
                    onLaunch(
                        JSONObject()
                            .put("addr", kh.address).put("port", kh.port).put("fp_hex", kh.fpHex)
                            .put("launch", link.launch ?: JSONObject.NULL)
                            .put("preset", presetRef?.let { presetStore.resolve(it).first?.id } ?: JSONObject.NULL)
                            .put("request_access", false),
                    )
                }
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

    /** Whether the console's input test (`ConsoleCmd::PadTest`) has the pad, not the menu. */
    internal fun padTesting(): Boolean = state.now { padTest != null }

    /** A pad key for the input test; false when no test is on and the menu takes it. */
    internal fun padTestKey(code: Int, down: Boolean): Boolean = state.now {
        val test = padTest ?: return@now false
        test.key(code, down)
        pushPadTest(test)
        true
    }

    /** A pad's axes for the input test; false when no test is on and the menu takes them. */
    internal fun padTestMotion(ev: MotionEvent): Boolean = state.now {
        val test = padTest ?: return@now false
        test.motion(ev)
        pushPadTest(test)
        true
    }

    private fun pushPadTest(test: PadTestReading) {
        if (handle != 0L) NativeBridge.nativeConsoleSetPadTest(handle, test.json())
    }

    // ---- model pushers -----------------------------------------------------------------------

    private fun pushHosts() {
        if (handle == 0L) return
        refreshHostState()
        NativeBridge.nativeConsoleSetHosts(
            handle,
            ConsoleJson.hostRows(
                services.store.all(), services.discovered, services.reachable, presetStore.all(),
                services.hostActions, services.nowPlaying,
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
        for (h in services.store.all()) {
            if (!h.paired || h.fpHex.isEmpty()) continue
            // Reachable means it answered the probe — an advert would say yes for a host that
            // is asleep, and this asks it a question only a live host can answer.
            if (h.id !in services.reachable) continue
            val (addr, mgmt, fp) = Triple(h.address, h.effectiveMgmtPort, h.fpHex)
            val port = h.port
            // Stamp BEFORE the request, so a slow host cannot make every push spawn another.
            if (now - (services.nowPlayingAt[fp] ?: 0L) >= NOW_PLAYING_TTL_MS) {
                services.nowPlayingAt[fp] = now
                ioPool.execute {
                    val up = LibraryClient.fetchRunning(addr, mgmt, id.certPem, id.privateKeyPem, fp, port)
                    state.launch { recordNowPlaying(fp, up) }
                }
            }
            if (now - (services.hostActionsAt[fp] ?: 0L) < HOST_ACTIONS_TTL_MS) continue
            services.hostActionsAt[fp] = now
            ioPool.execute {
                val found = HostActions.list(id, addr, mgmt, fp, port)
                state.launch {
                    services.hostActions[fp] = found
                    pushHosts()
                }
            }
        }
    }

    /**
     * Adopt a `/status` answer as the carousel's "▶ <title>" line: the first entry that is up
     * and has a title to show, else nothing. Re-pushes only on a real change, so a host
     * answering every 20 s does not churn the snapshot generation.
     */
    internal fun recordNowPlaying(fpHex: String, games: List<RunningGame>) {
        val title = games.firstOrNull { it.isUp && it.title.isNotEmpty() }?.title.orEmpty()
        if (services.nowPlaying[fpHex] == title) return
        services.nowPlaying[fpHex] = title
        pushHosts()
    }

    private fun pushKnownHosts() {
        if (handle == 0L) return
        NativeBridge.nativeConsoleSetKnownHosts(handle, ConsoleJson.knownHosts(services.store.all()))
    }

    internal fun notice(text: String) {
        if (handle != 0L) NativeBridge.nativeConsoleNotice(handle, text)
    }

    // ---- events from the console ---------------------------------------------------------

    private fun ConsoleState.onEvent(ev: JSONObject) {
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

    private fun ConsoleState.onSettingsSaved(j: JSONObject) {
        appContext?.getSharedPreferences(PREFS, Context.MODE_PRIVATE)?.edit()
            ?.putString("json", j.toString())?.apply()
        val next = ConsoleJson.applySettings(settings, j)
        if (next != settings) adoptSettings(next)
    }

    /** Store [next] and hand it to the app, whose [settingsChanged] pushes it back to the console. */
    private fun ConsoleState.adoptSettings(next: Settings) {
        settings = next
        settingsStore.save(next)
        onSettingsChange?.invoke(next)
    }

    private fun ConsoleState.onAction(action: Any) {
        when (action) {
            is String -> when (action) {
                "Quit" -> onQuit?.invoke()
                "CancelConnect" -> {
                    dial?.cancel()
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
                action.optJSONObject("Launch")?.let { onLaunch(it) }
                action.optString("CopyText").takeIf { action.has("CopyText") }?.let { text ->
                    val cm = appContext?.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
                    cm?.setPrimaryClip(ClipData.newPlainText("punktfunk", text))
                }
                action.optJSONObject("PinShortcut")?.let(::pinShortcut)
                action.optJSONObject("SaveLauncherFile")?.let { a ->
                    onSaveLauncherFile?.invoke(a.optString("title"), a.optString("url"))
                }
            }
        }
    }

    /**
     * `OverlayAction::PinShortcut`: the title's link as a home-screen shortcut. The record and
     * the cached shelf the link names give the shortcut its key and its poster.
     */
    private fun pinShortcut(a: JSONObject) {
        val app = appContext ?: return
        val url = a.optString("url")
        val title = a.optString("title")
        val link = (DeepLinks.parse(url) as? DeepLinkResult.Parsed)?.link ?: return
        val host = (DeepLinks.resolveHost(link, services.store.all()) as? HostResolution.Record)?.host ?: return
        val me = identity
        ioPool.execute {
            val posters = link.launch?.let { id ->
                LibraryCache.standard(app.cacheDir).load(LibraryCache.keyFor(host, host.fpHex))
                    ?.games?.firstOrNull { it.id == id }?.art?.posterCandidates
            }.orEmpty()
            val key = shortcutId(host.id, link.launch, link.preset)
            pinTitleShortcut(app, me, host, posters, url, title, key)?.let { state.launch { notice(it) } }
        }
    }

    /**
     * `OverlayAction::Launch` — the console asked for a session. The trust decision was the
     * console's (an unpaired host went to its Pair screen first), so this is the dial itself
     * ([HostServices.dial]): pinned by the row's fingerprint, with the host's bound preset or the
     * pinned card's one-off, and — for the pair screen's "Request access" — the long approval
     * budget. The result lands on main, beside `CancelConnect`.
     */
    private fun ConsoleState.onLaunch(a: JSONObject) {
        val app = appContext ?: return
        val fp = a.optString("fp_hex")
        // The record the row's pin names — never the other OS saved at the same address — and
        // dialled where IT says: the last sweep may have followed the host to a new address. An
        // unpinned row is the placeholder at its address.
        val kh = services.store.resolve(fp, a.optString("addr"), a.optInt("port"))
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
        val ticket = HostServices.Ticket()
        dial = ticket
        NativeBridge.nativeConsoleSessionPhase(handle, 0, "")
        pauseDiscovery() // the Wi-Fi radio belongs to the stream session now
        val spec = DialSpec(
            effective, id, addr, port, fp, launchId,
            dialer = if (launchId != null) "console/library" else "console/desktop",
            timeoutMs = if (requestAccess) REQUEST_ACCESS_TIMEOUT_MS else CONNECT_TIMEOUT_MS,
            preset = preset,
            profile = profile,
        )
        // A host the dial saves by what it presented is named by its advert, else its address.
        val name = services.discovered.firstOrNull { it.host == addr && it.port == port }?.name ?: addr
        // A dial the host parks for its operator narrates the approval wait (phase 6).
        val watch = state.launch {
            while (!NativeBridge.nativeDialPending()) kotlinx.coroutines.delay(PENDING_POLL_MS)
            NativeBridge.nativeConsoleSessionPhase(handle, 6, "")
        }
        state.launch {
            val dialed = services.dial(spec, kh, name, requestAccess, ticket)
            watch.cancel()
            if (dialed is HostServices.Cancelled) return@launch
            dial = null
            if (dialed is HostServices.Connected) {
                // A request-access approval, or a first dial to a host not saved yet, saved what
                // the host presented, so the next connect is silent.
                if (dialed.pinned) {
                    pushHosts(); pushKnownHosts()
                }
                val h = dialed.session.handle
                val session = dialed.session
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
            } else if (dialed is HostServices.Failed) {
                val token = dialed.token
                if (token == "profile-unknown" && kh != null) {
                    HostRecords.savePick(services.store, kh, null)
                    pushHosts(); pushKnownHosts()
                }
                // 5: the console forgets the pick and asks the box's list once more.
                NativeBridge.nativeConsoleSessionPhase(
                    handle, if (token == "profile-unknown") 5 else 2,
                    dialed.said.ifEmpty { ConnectErrors.connectMessage(token, requestAccess) },
                )
                resumeDiscovery()
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
            state.launch { if (handle != 0L) NativeBridge.nativeConsoleSetLicenses(handle, json) }
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
        // The console closed the card itself; a `null` here could close the next wake's.
        "CancelWake" to { _ -> services.wakes.next() },
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
        "FetchAvatar" to { c -> fetchAvatar(c) },
        "WakeProfile" to ::wakeProfile,
        "SetProfile" to ::setProfile,
        "BindPreset" to ::bindPreset,
        "SetClipboard" to ::setClipboard,
        "PadTest" to { c -> state.now { padTest = if (c.optBoolean("on")) PadTestReading() else null } },
        "PadAction" to { c -> state.now { onPadAction }?.invoke(c.optString("action"), c.optString("pad_key")) },
    )

    /** `ConsoleCmd`s this shell drops: it raises no prompt, so no `PromptAnswer` comes back. */
    internal val ignoredCommands = setOf("PromptAnswer")

    /**
     * The stream port of the saved host [fp] pins, where its management API answers without TCP.
     * 0 with no such record: HTTPS alone. A command names the mgmt port, not this.
     */
    internal fun streamPort(fp: String): Int = services.store.getByFp(fp)?.port ?: 0

    private fun hostForKey(key: String): KnownHost? {
        val primary = ConsoleJson.hostKey(key)
        return services.store.all().firstOrNull { ConsoleJson.rowKey(it.fpHex, it.address, it.port) == primary }
    }

    /**
     * A manual entry has no pin yet: it renames the placeholder at its address or adds one. A
     * record pinned there is another identity — the other OS of a dual-boot box — and keeps its name.
     */
    private fun saveHost(c: JSONObject) {
        val addr = c.optString("addr"); val port = c.optInt("port"); val name = c.optString("name")
        val existing = services.store.placeholderAt(addr, port)
        if (existing != null) {
            if (name.isNotEmpty()) services.store.save(existing.copy(name = name))
        } else {
            services.store.save(
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
        if (addr != kh.address || port != kh.port) services.store.remove(kh)
        services.store.save(kh.copy(name = name.ifEmpty { addr }, address = addr, port = port))
        pushHosts(); pushKnownHosts()
    }

    /**
     * `ConsoleCmd::ForgetHost` — [HostRecords.forget]. A cleared default host is saved the way
     * the console's own settings edits are.
     */
    private fun forgetHost(c: JSONObject) {
        val app = appContext ?: return
        val kh = hostForKey(c.optString("key")) ?: return
        state.now { HostRecords.forget(app, services.store, kh, settings)?.let { adoptSettings(it) } }
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
        HostRecords.unpair(services.store, kh)
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::BindPreset` — [HostRecords.bindPreset]; a null `preset_id` clears. */
    private fun bindPreset(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        val pid = c.optString("preset_id")
            .takeIf { c.has("preset_id") && !c.isNull("preset_id") && it.isNotEmpty() }
        val game = c.optString("game")
            .takeIf { c.has("game") && !c.isNull("game") && it.isNotEmpty() }
        HostRecords.bindPreset(services.store, kh, pid, game)
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::SetClipboard` — [HostRecords.setClipboard]. */
    private fun setClipboard(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        HostRecords.setClipboard(services.store, kh, c.optBoolean("on"))
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
                HostProfiles.fetch(id, addr, mgmt, fp, streamPort(fp))
            }
            val json = ConsoleJson.profilesAnswer(answer)
            state.launch { if (handle != 0L) NativeBridge.nativeConsoleSetProfiles(handle, fp, json) }
        }
    }

    /** `ConsoleCmd::WakeProfile`: start a stopped seat. No answer: the shell polls `FetchProfiles`. */
    private fun wakeProfile(c: JSONObject) {
        val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
        val profile = c.optString("id")
        val id = identity ?: return
        ioPool.execute { HostProfiles.wake(id, addr, mgmt, fp, profile, streamPort(fp)) }
    }

    /** `ConsoleCmd::SetProfile`: save (or with no profile, clear) the pick on a host. */
    private fun setProfile(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        HostRecords.savePick(services.store, kh, ProfilePick.fromJson(c.optJSONObject("profile")))
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::SetPin` — [HostRecords.setPin]. */
    private fun setPin(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        HostRecords.setPin(services.store, kh, c.optString("preset_id"), c.optBoolean("pin"))
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
            val message = io.unom.punktfunk.SendLogs.toHost(app, id, addr, mgmt, fp, hostName, streamPort(fp))
            state.launch { notice(message) }
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
        // The probe blocks for its two-second burst on the IO pool, and the console keeps
        // drawing.
        ioPool.execute {
            kotlinx.coroutines.runBlocking {
                runSpeedTest(
                    services, id, addr, port, fp,
                    onProgress = { kbps -> state.launch { advanceSpeedProgress(key, kbps) } },
                ) { p -> state.launch { advanceSpeed(key, p) } }
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
            val message = HostActions.invoke(id, addr, mgmt, fp, hostName, actionId, label, streamPort(fp))
            state.launch { notice(message) }
        }
    }

    private fun pair(c: JSONObject) {
        val pin = c.optString("pin"); val name = c.optString("device_name")
        runPairing(c, ConnectErrors::pairMessage) { id, addr, port ->
            services.pair(addr, port, id, pin, name)
        }
    }

    /** No PIN: wait for the host's operator, pinned to the advertised fingerprint. Never streams. */
    private fun requestAccess(c: JSONObject) {
        val fpHex = c.optString("fp_hex"); val name = c.optString("device_name")
        runPairing(c, { ConnectErrors.connectMessage(it, requestAccess = true) }) { id, addr, port ->
            services.requestAccess(addr, port, id, fpHex, name)
        }
    }

    /**
     * One pairing off the main thread, reported Busy, then Paired or Failed, under the command's
     * `req`. A later pairing makes this one's answer stale ([HostServices.pairings]).
     */
    private fun runPairing(
        c: JSONObject,
        wording: (String) -> String,
        ask: (ClientIdentity, String, Int) -> HostServices.Paired,
    ) {
        val addr = c.optString("addr"); val port = c.optInt("port")
        val req = c.optJSONObject("req") ?: JSONObject()
        val id = identity
        if (id == null) {
            NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairFailed(req, identities.blockedMessage()))
            return
        }
        val gen = services.pairings.next()
        NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairBusy(req))
        ioPool.execute {
            val paired = ask(id, addr, port)
            state.launch {
                if (!services.pairings.isCurrent(gen)) return@launch
                when (paired) {
                    is HostServices.Paired.Ok -> {
                        val fp = paired.fp
                        // Named once the ceremony says who answered: the address may carry both
                        // OS installs of a dual-boot box, and the first record there is not this one.
                        val live = services.discovered
                        val hostName = services.store.resolve(fp, addr, port)?.name
                            ?: live.firstOrNull { it.fingerprint.equals(fp, true) }?.name
                            ?: live.firstOrNull { it.fingerprint == null && it.host == addr && it.port == port }?.name
                            ?: addr
                        services.trustPaired(addr, port, hostName, fp)
                        pushHosts(); pushKnownHosts()
                        NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairPaired(req, fp))
                    }
                    is HostServices.Paired.Failed ->
                        NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairFailed(req, wording(paired.token)))
                }
            }
        }
    }

    /**
     * The wake-and-wait loop ([HostServices.wake], the desktop's `spawn_wake`), each status under
     * the command's `req`; the console reads `online`/`timed_out` off it and acts (a `then_connect`
     * wake dials from the shell's side once online). Online is a probe of the host, at its live
     * advert's address if it has one.
     */
    private fun wake(c: JSONObject) {
        val key = c.optString("key"); val thenConnect = c.optBoolean("then_connect")
        val req = c.optJSONObject("req") ?: JSONObject()
        val kh = hostForKey(key) ?: return
        if (kh.mac.isEmpty()) return
        val claim = services.wakes.next()
        val name = kh.name.ifBlank { kh.address }
        ioPool.execute {
            services.wake(
                claim, kh.mac, kh.address,
                isOnline = { services.answers(kh, services.discovered.firstOrNull { kh.matches(it) }, 900) },
                cancelled = { handle == 0L },
            ) { seconds, timedOut, online ->
                NativeBridge.nativeConsoleSetWake(
                    handle,
                    ConsoleJson.wakeStatus(req, key, name, seconds, timedOut, online, thenConnect),
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
            KeyEvent.KEYCODE_BUTTON_C to "L4",
            KeyEvent.KEYCODE_BUTTON_Z to "R4",
            KeyEvent.KEYCODE_DPAD_UP to "Up",
            KeyEvent.KEYCODE_DPAD_DOWN to "Down",
            KeyEvent.KEYCODE_DPAD_LEFT to "Left",
            KeyEvent.KEYCODE_DPAD_RIGHT to "Right",
        )
    }
}
