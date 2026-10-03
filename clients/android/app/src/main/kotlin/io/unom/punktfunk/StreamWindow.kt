package io.unom.punktfunk

import android.content.Context
import android.content.pm.ActivityInfo
import android.hardware.display.DisplayManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.Display
import android.view.View
import android.view.WindowManager
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import io.unom.punktfunk.kit.NativeBridge

/** Touch and mouse, sticks and triggers, captured mouse-look, captured touchpad. */
internal const val STREAM_UNBUFFERED_SOURCES = android.view.InputDevice.SOURCE_CLASS_POINTER or
    android.view.InputDevice.SOURCE_CLASS_JOYSTICK or
    android.view.InputDevice.SOURCE_CLASS_TRACKBALL or
    android.view.InputDevice.SOURCE_CLASS_POSITION

/** The display's modes at its current resolution, which is the one the user picked. */
internal fun Display.sameResolutionModes(): List<Display.Mode> {
    val current = mode
    return supportedModes.filter {
        it.physicalWidth == current.physicalWidth && it.physicalHeight == current.physicalHeight
    }
}

/** One row of a display's mode table, landscape. */
internal data class PanelMode(val id: Int, val width: Int, val height: Int, val hz: Float)

/**
 * The mode a [hz] stream of [size] pins: the panel's own size when the stream is exactly that and
 * [current] renders below it, else [current]'s size. At that size, the exact rate, else the
 * smallest integer multiple (120 for a 60 stream: judder-free 2:1 pulldown), else the highest.
 */
internal fun pickStreamMode(
    modes: List<PanelMode>,
    current: PanelMode,
    hz: Int,
    size: Pair<Int, Int>?,
): PanelMode? {
    if (hz <= 0) return null
    val own = current.width to current.height
    val panel = Resolutions.panelScreens(own, own, modes.map { it.width to it.height }).first
    val at = if (size == panel) panel else own
    fun multiple(rate: Float): Int {
        val k = (rate / hz).toInt()
        return if (k >= 2 && kotlin.math.abs(rate - hz * k) < 1f) k else 0
    }
    return modes.filter { it.width to it.height == at }.minWithOrNull(
        compareBy(
            {
                when {
                    kotlin.math.abs(it.hz - hz) < 1f -> 0 // exact
                    multiple(it.hz) > 0 -> 1 // integer multiple — prefer smallest
                    else -> 2 // no relation — prefer highest
                }
            },
            { if (multiple(it.hz) > 0) it.hz else -it.hz },
        ),
    )
}

private fun Display.Mode.row() = PanelMode(
    modeId,
    maxOf(physicalWidth, physicalHeight),
    minOf(physicalWidth, physicalHeight),
    refreshRate,
)

/** [pickStreamMode] on this display. [size] is the stream's, or null to stay at the current size. */
internal fun Display.streamModeFor(hz: Int, size: Pair<Int, Int>? = null): Display.Mode? {
    val id = pickStreamMode(supportedModes.map { it.row() }, mode.row(), hz, size)?.id ?: return null
    return supportedModes.firstOrNull { it.modeId == id }
}

/**
 * The panel refresh a [hz] stream of [size] runs against on this display, from the mode TABLE:
 * `refreshRate` reports a per-uid frame-rate override (games get 60 on Android 15+), not the
 * panel. `0` when unresolvable.
 */
internal fun Display.streamPanelFps(hz: Int, size: Pair<Int, Int>? = null): Int =
    streamModeFor(hz, size)?.refreshRate?.let { kotlin.math.round(it).toInt() } ?: 0

/**
 * Everything a stream does to the activity's WINDOW, and how to put it back: the wake and Wi-Fi
 * locks, the Wi-Fi link log, the panel's refresh pin, HDMI ALLM, the soft-keyboard and cutout
 * modes, the landscape lock, unbuffered input dispatch and the render-rate vote.
 *
 * It is one object because it is one obligation — every field below is a prior value captured on the
 * way in, and [detach] is the only thing that ever restores one. Held in [StreamScreen]'s session
 * `DisposableEffect`, whose lifetime it shares exactly.
 *
 * Two entry points rather than one because the order is load-bearing: [attach] runs before the
 * session's input peripherals are built, [pinDisplay] after, and both sit where they always did.
 * Nothing here reads Compose state or writes any, which is what lets it be a plain class.
 */
internal class StreamWindow(
    private val activity: MainActivity?,
    private val context: Context,
    /** The view hosting the composition — the one unbuffered dispatch and the vote apply to. */
    private val composeView: View,
    private val lowLatencyMode: Boolean,
    private val isTv: Boolean,
    /** The negotiated stream refresh (0 = unknown / older native lib). */
    private val streamHz: Int,
    /** The negotiated stream size, landscape; null when unknown. */
    private val streamSize: Pair<Int, Int>?,
) {
    private val window = activity?.window
    private val controller = window?.let { WindowCompat.getInsetsController(it, it.decorView) }
    private val wifiManager =
        context.applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager

    /**
     * Wi-Fi locks held for the stream's duration — BOTH of them, unconditionally (Moonlight does
     * the same). Without an effective lock, Wi-Fi power save batches downlink delivery into
     * beacon-interval clumps: hundreds of ms of latency mush, sawtoothing bitrate, and periodic
     * whole-frame loss when the AP's power-save buffer overflows (all observed live on a phone).
     *  - FULL_LOW_LATENCY (API 29+) is the only lock that actually disables power save on modern
     *    Android; it needs the app foreground + screen on, which a stream always is.
     *  - FULL_HIGH_PERF covers older releases — it is deprecated AND a documented no-op on recent
     *    Android, which is exactly why it can't be the only lock (a lesson learned: holding just
     *    HIGH_PERF left power save fully active on Android 13+).
     * Non-reference-counted: one explicit acquire/release each.
     */
    private val wifiLocks: List<WifiManager.WifiLock> = run {
        val wm = wifiManager ?: return@run emptyList()
        buildList {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                wm.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "punktfunk:stream-ll")
                    ?.let(::add)
            }
            @Suppress("DEPRECATION")
            wm.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "punktfunk:stream-hp")
                ?.let(::add)
        }.onEach { it.setReferenceCounted(false) }
    }

    private val displayManager =
        context.getSystemService(Context.DISPLAY_SERVICE) as? DisplayManager

    /**
     * One `pf.display` line in the log ring per panel change for the stream's lifetime. `mode=` is
     * the panel's active mode, `render=` what this app is allowed: `mode=120 render=60` is a
     * per-uid frame-rate override, `mode=60` a real mode switch. Below Android 13 the native
     * presenter's period is the requested mode, so this line is the bundle's only witness of a
     * slower panel.
     *
     * A display arriving or leaving writes the whole list to the log ring instead ([logDisplays]).
     */
    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(displayId: Int) = logDisplays("added $displayId")
        override fun onDisplayRemoved(displayId: Int) = logDisplays("removed $displayId")
        override fun onDisplayChanged(displayId: Int) = logPanel("changed")
    }

    /** The last panel line written; `onDisplayChanged` also fires for changes that keep the rate. */
    private var lastPanel = ""

    private fun logPanel(why: String) {
        val d = runCatching { activity?.display }.getOrNull() ?: return // API 30; hidden below
        val panel = "mode=${d.mode.refreshRate} render=${d.refreshRate}"
        if (panel == lastPanel) return
        lastPanel = panel
        runCatching { NativeBridge.nativeLogDisplay("panel $why $panel") }
    }

    /** Every display, into the "Send logs" bundle: which dual-screen shape this device reports. */
    private fun logDisplays(why: String) {
        val dm = displayManager ?: return
        runCatching { NativeBridge.nativeLogDisplay("displays $why: ${describeDisplays(context, dm)}") }
    }

    /**
     * The Wi-Fi link read once a second on the main thread, logged through the native ring
     * ([WifiLinkLog]) so a field bundle can tell a radio problem from a stream one. Stops in [detach].
     */
    private val wifiLinkLog = WifiLinkLog()
    private val mainHandler = Handler(Looper.getMainLooper())
    private val pollWifiLink = object : Runnable {
        override fun run() {
            val link = wifiManager?.readLink() ?: return
            wifiLinkLog.reason(link, SystemClock.elapsedRealtime())?.let { why ->
                runCatching {
                    NativeBridge.nativeLogWifiLink(
                        why, link.rssiDbm, link.txMbps, link.rxMbps, link.freqMhz, link.standard,
                    )
                }
            }
            mainHandler.postDelayed(this, 1_000)
        }
    }

    private var priorSoftInput = WindowManager.LayoutParams.SOFT_INPUT_ADJUST_UNSPECIFIED
    private var priorCutout: Int? = null
    private var priorOrientation: Int? = null

    /** Window state the stream wants from its first frame, before any peripheral exists. */
    fun attach() {
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        // acquire() ENFORCES the WAKE_LOCK permission (manifest) — and a failed acquire MUST be
        // loud: a silent runCatching hid the missing permission for weeks (dumpsys wifi showed
        // low_latency_active_time_ms=0 across every "locked" stream).
        wifiLocks.forEach { lock ->
            runCatching { lock.acquire() }.onFailure { e ->
                Log.w("punktfunk", "WifiLock acquire failed — power save stays ON: $lock", e)
            }
        }
        mainHandler.post(pollWifiLink)
        // HDMI Auto Low-Latency Mode: ask the display to drop its post-processing (game mode) —
        // the biggest panel-side latency win on the TV boxes. No-op where ALLM isn't supported. API
        // 30+. Part of the experimental low-latency stack.
        if (lowLatencyMode && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window?.setPreferMinimalPostProcessing(true)
        }
        // System bars: NOT hidden here — App.kt owns hide/show (one owner; the AnimatedContent
        // handoff broke per-screen ownership, see the `immersive` effect there).
        // The soft keyboard (three-finger swipe up → KeyCaptureView) must OVERLAY the stream, never
        // pan/resize it — the video is a fixed-mode surface, not a document. Scoped to the stream;
        // the app's other screens keep the default for their text fields.
        priorSoftInput = window?.attributes?.softInputMode
            ?: WindowManager.LayoutParams.SOFT_INPUT_ADJUST_UNSPECIFIED
        window?.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING)
        // Draw under the display cutout, explicitly. Android 15's SDK-35 edge-to-edge enforcement
        // makes ALWAYS the immersive default, but pre-15 devices letterbox the notch as a dead
        // black bar unless asked — and the stream's own letterbox is black anyway, so the cutout
        // region can never show anything wrong.
        priorCutout = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window?.attributes?.layoutInDisplayCutoutMode
        } else {
            null
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window?.let { w ->
                w.attributes = w.attributes.apply {
                    layoutInDisplayCutoutMode =
                        WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
                }
            }
        }
        // Lock to landscape while streaming — the host streams a landscape desktop, so pin the
        // device there (either direction is fine) and stop it rotating to portrait mid-session. The
        // activity declares configChanges=orientation, so this re-lays out the surface in place
        // without recreating the activity (no stream restart). On TV it is a harmless no-op.
        //
        // COMPACT devices only (sw < 600 dp): on tablets/foldables/desktop windows the lock is a
        // large-display anti-pattern (Play flags it; Android 16+ ignores it there outright), and the
        // stream doesn't need it — the aspect-ratio letterbox renders correctly in any orientation,
        // the lock is purely a phone-ergonomics choice.
        val compactDevice = context.resources.configuration.smallestScreenWidthDp < 600
        priorOrientation = activity?.requestedOrientation
        if (compactDevice) {
            activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        }
    }

    /**
     * Pin the panel to the stream's refresh (exact / multiple) for the session, and to the panel's
     * own size when the stream is that size ([pickStreamMode]). [detach] and leaving the foreground
     * drop the pin. The decoder's own `ANativeWindow_setFrameRate` hint still aligns vsync, but it is
     * advisory — some OEM refresh governors ignore it outright and would leave a 120 Hz session on a
     * 60/90 Hz panel. TV boxes skip the pin: the native side actively drives the HDMI mode there.
     *
     * Also takes pointer events off the vsync batch and votes the app's render rate up, both of
     * which the stream pays a frame of latency for otherwise.
     */
    fun pinDisplay() {
        if (isTv) {
            activity?.setConsoleHighRefreshRate(false) // the decoder's HDMI mode switch governs
        } else {
            activity?.setStreamDisplayMode(streamHz, streamSize)
        }
        // Moves are vsync-batched by default — up to a frame of input latency the stream
        // shouldn't pay. Unbuffered dispatch delivers them the moment the kernel does. A focus
        // change below this view recomputes its request, so [KeyCaptureView] holds one too.
        // Undone by passing 0 on the way out (API 30+).
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            composeView.requestUnbufferedDispatch(STREAM_UNBUFFERED_SOURCES)
        }
        // Vote the app's RENDER rate up to the stream's (API 35+). The mode pin above governs the
        // panel, but the platform separately down-rates a quiet app's choreographer stream
        // (frame-rate categories: a non-animating UI reads as "normal" = 60) — observed on-glass
        // as 16.6 ms vsync callbacks on a 120 Hz panel, which would pace the presenter at half
        // rate. The native side also subdivides onto the panel grid, so this vote is the belt to
        // that braces. Reset to no-preference on the way out.
        if (Build.VERSION.SDK_INT >= 35 && streamHz > 0) {
            composeView.requestedFrameRate = streamHz.toFloat()
        }
        displayManager?.registerDisplayListener(displayListener, null)
        logPanel("pinned")
        logDisplays("at start")
    }

    /** Put every prior value back, in the order the stream took them. */
    fun detach() {
        displayManager?.unregisterDisplayListener(displayListener)
        activity?.setConsoleHighRefreshRate(true) // back to the console UI's max refresh
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            composeView.requestUnbufferedDispatch(0) // back to ordinary batched dispatch
        }
        if (Build.VERSION.SDK_INT >= 35) {
            composeView.requestedFrameRate = View.REQUESTED_FRAME_RATE_CATEGORY_DEFAULT
        }
        controller?.hide(WindowInsetsCompat.Type.ime()) // drop any keyboard left showing
        window?.setSoftInputMode(priorSoftInput)
        priorCutout?.let { prior ->
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                window?.let { w ->
                    w.attributes = w.attributes.apply { layoutInDisplayCutoutMode = prior }
                }
            }
        }
        window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        if (lowLatencyMode && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window?.setPreferMinimalPostProcessing(false)
        }
        wifiLocks.forEach { runCatching { if (it.isHeld) it.release() } }
        mainHandler.removeCallbacks(pollWifiLink)
        // Release the landscape lock so the rest of the app follows the device/system again.
        activity?.requestedOrientation =
            priorOrientation ?: ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
    }
}
