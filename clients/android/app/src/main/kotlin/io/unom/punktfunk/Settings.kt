package io.unom.punktfunk

import android.content.Context
import android.hardware.display.DisplayManager
import android.os.Build
import android.util.Log
import android.view.Display
import android.view.WindowInsets
import android.view.WindowManager
import androidx.core.content.ContextCompat

/**
 * User-tunable stream settings, persisted in `SharedPreferences`. A `0` resolution/refresh means
 * "native display mode" (resolved at connect time from [nativeDisplayMode]); `0` bitrate means the
 * host's default. [compositor]/[gamepad] are the `CompositorPref`/`GamepadPref` wire bytes the host
 * understands (0 = Auto). Mirrors the Linux/Apple clients' settings.
 */
data class Settings(
    val width: Int = 0,
    val height: Int = 0,
    val hz: Int = 0,
    val bitrateKbps: Int = 0,
    /** PyroWave quality, bits per pixel from 0.5 to 2: the most a PyroWave session sends. Shown
     *  as the rate it needs ([PyroWaveQuality]), never as bits per pixel. */
    val pyrowaveBpp: Double = 1.6,
    /**
     * Render-resolution multiplier: the client asks the host to render/encode at `chosen mode ×
     * renderScale` and the compositor downscales the larger decoded frame to the SurfaceView
     * (`> 1` supersamples for sharpness, at more bandwidth AND decode; `< 1` renders under native
     * for a lighter host/link). `1.0` = Native. Applied at connect via [RenderScale.apply], clamped
     * even + to the codec's max dimension. Mirrors the Apple/Linux clients' render scale.
     */
    val renderScale: Double = 1.0,
    /**
     * How a frame whose shape differs from the screen fills it: `"fit"` (whole picture, bars),
     * `"crop"` or `"stretch"`. The cross-client `video_fit` key; unknown reads as fit
     * ([io.unom.punktfunk.kit.VideoFit.fromName]).
     */
    val videoFit: String = "fit",
    /**
     * Advertise HDR (10-bit BT.2020 PQ) to the host. Default on, but only *effective* on a panel that
     * can actually present HDR10 (see [displaySupportsHdr]) — on an SDR display HDR is never
     * advertised regardless, so the host sends a proper 8-bit BT.709 stream rather than PQ the panel
     * would mis-tone-map. Turning this off forces SDR even on a capable panel.
     */
    val hdrEnabled: Boolean = true,
    /**
     * Ask for 10-bit WITHOUT HDR — Main10 at BT.709. Off by default, and subsumed by
     * [hdrEnabled], which already implies 10 bits.
     *
     * Unlike HDR this asks nothing of the panel: an 8-bit display shows a dithered Main10 stream
     * perfectly well, and the gain is banding-free gradients — skies, fades, dark scenes — for a
     * little bandwidth. So it is never gated on [displaySupportsHdr]. Mirrors the cross-client
     * `ten_bit_sdr` key the desktop clients write.
     */
    val tenBitSdr: Boolean = false,
    val compositor: Int = 0,
    val gamepad: Int = 0,
    /**
     * Forward this device's controllers to the host at all. Default on — that was the
     * unconditional behaviour before this became a setting.
     *
     * Off is for a couch whose controller reaches the host another way: a USB passthrough tool
     * (VirtualHere and friends), or a pad simply plugged into the host itself. Leaving it on
     * there gives the host two controllers for one pair of hands, and games read both. It also
     * stops this device CLAIMING the pad — a device held open is one a passthrough tool can't
     * bind — which is why it gates the USB capture paths, not just the wire sends.
     */
    val gamepadForwarding: Boolean = true,
    /**
     * Where the guide (Xbox/PS) and misc/share presses land while streaming — the
     * cross-client `system_buttons` key: `"auto"` (forward on Android — the press reaches
     * the app on most devices) | `"forward"` | `"local"`.
     */
    val systemButtons: String = "auto",
    /**
     * The hold-Select guide gesture — the cross-client `guide_gesture` key: `"auto"` (off
     * on Android) | `"on"` | `"off"`. On: holding Select alone ≥350 ms sends the HOST's
     * guide, down until release (long hold = the host's long-press → a Gaming-Mode host's
     * QAM); a Select tap is delivered on release, slightly delayed. For devices whose
     * shell intercepts the physical guide button.
     */
    val guideGesture: String = "auto",
    /** Requested audio channel count: 2 (stereo), 6 (5.1) or 8 (7.1). The host clamps to what it
     * can capture; the resolved count drives the decoder + AAudio layout. */
    val audioChannels: Int = 2,
    /**
     * Requested audio format — the cross-client `audio_format` key: [AUDIO_FORMAT_OPUS] (the
     * default, and byte-for-byte the session every build before the lossless plane ran) or one of
     * the lossless rows in [AUDIO_FORMAT_OPTIONS], which span both rate families.
     *
     * Off by default and deliberately: lossless takes 2.1–8.5 Mbps off the top of the link,
     * OUTSIDE the ABR loop that manages the video budget, against the ~256 kbps Opus it replaces —
     * so a user has to pick it. Since 2026-08-17 this setting is the ONLY opt-in: the host's half
     * (`PUNKTFUNK_AUDIO_HIRES`) defaults ON and is an opt-OUT (`=0`), so this choice is enough on
     * any host that has not deliberately turned the plane off.
     * A REQUEST, never a fact: the host runs its gate and may answer Opus anyway, and
     * the native side downgrades the rate first if THIS device will not open it. What actually
     * happened is on the stats HUD, and in logcat's `audio: plane codec=… rate=…` line.
     */
    val audioFormat: String = AUDIO_FORMAT_OPUS,
    /** Preferred video codec: `"auto"` (host decides), `"hevc"`, `"h264"`, or `"av1"`. A soft
     * preference — the host emits it when it can, else falls back. AMediaCodec decodes whichever
     * the host resolves (AV1 is only advertised/offered when the device has a real AV1 decoder). */
    val codec: String = "auto",
    /**
     * A second, smaller screen — a dual-screen handheld's lower panel, a foldable half open —
     * shows the companion panel while streaming, or the picture (design/android-dual-screen.md).
     * Off keeps the stream on this screen and leaves the other to the system: the phone on a
     * TV that the size rule still takes for a handheld's panel. Presetable, so a TV preset
     * carries it.
     */
    val secondScreen: Boolean = true,
    val micEnabled: Boolean = false,
    /**
     * Cancel acoustic echo on the mic uplink (plus noise suppression): the capture opens under
     * the VoiceCommunication preset so the HAL's own AEC/NS process it, with the Java effects
     * attached as a backstop where available. On by default — a phone/tablet plays the game audio
     * out of the same device its mic hears, so without this the host hears its own stream back.
     * Turn off for a headset-only setup where the untouched full-band capture sounds better.
     * Only meaningful while [micEnabled] is on.
     */
    val echoCancel: Boolean = true,
    /**
     * Ask the host to leave ITS OWN audio devices alone for this session
     * (`CLIENT_CAP_KEEP_HOST_AUDIO`): it captures whatever its default playback device already is,
     * so the speakers or headphones on the host PC keep playing while this device hears the same
     * audio. Off — the default, and what every build before this did — has the host park playback
     * on a silent endpoint, which is why the host goes quiet the moment a stream starts.
     *
     * REQUEST-only: there is no host-cap echo, so an older host ignores the ask and re-routes as it
     * always did ("audio still works, the host went quiet"), never a broken session.
     */
    val keepHostAudio: Boolean = false,
    /**
     * How much the in-stream stats overlay shows — see [StatsVerbosity]. Defaults to
     * [StatsVerbosity.NORMAL] (the res/fps line + latency headline + reliability counters); the full
     * decoder/feed/equation HUD is [StatsVerbosity.DETAILED], and a single terse line is
     * [StatsVerbosity.COMPACT]. A 3-finger tap cycles through the tiers live.
     */
    val statsVerbosity: StatsVerbosity = StatsVerbosity.NORMAL,
    /**
     * Which vocabulary the stats overlay speaks: off (the default) shows the figures Moonlight's
     * overlay also shows, on shows capture to glass and every stage. Device-wide; a preset never
     * carries it.
     */
    val advancedStats: Boolean = false,
    /** The stats overlay's corner, a cross-client `hud_placement` name; "" = top left. */
    val hudPlacement: String = "",
    /** The stats overlay's size in percent, on top of the display density. The overlay only. */
    val statsScalePct: Int = 100,
    /** Show how to leave for a few seconds when a stream starts. */
    val exitHint: Boolean = true,
    /** Settings show their advanced rows. Hiding a row keeps its value. */
    val showAdvanced: Boolean = false,
    /**
     * Touch input model — how touchscreen fingers drive the host. [TouchMode.TRACKPAD] (default):
     * the cursor stays put on touch-down and moves by the finger's relative delta (swipe to nudge,
     * lift and re-swipe to walk it across), tap to click where it is. [TouchMode.POINTER]: the
     * cursor jumps to the finger (direct pointing). [TouchMode.TOUCH]: real multi-touch
     * passthrough — every finger reaches the host as a touchscreen contact, for apps/games that
     * understand touch. [TouchMode.OFF]: fingers reach the host as nothing; the ring twist and
     * the three-finger gestures still work. Mirrors the Apple client's TouchInputMode.
     */
    val touchMode: TouchMode = TouchMode.TRACKPAD,
    /**
     * Swap the whole home screen for the controller-optimized "console" UI (the host carousel +
     * gamepad chrome) — mirrors the Apple client's `gamepadUIEnabled`. On by default; turn it off
     * to keep the touch UI even with a pad attached. WHEN it takes over is [gamepadUiMode].
     * A TV (leanback) is always in this mode regardless (its remote/pad is the only input).
     */
    val gamepadUiEnabled: Boolean = true,
    /**
     * Draw the console UI at 1080p and let the display scale it up, instead of at the panel's own
     * resolution. On by default on a TV, off on a phone — a deliberate sharpness-for-smoothness
     * trade, imposed only where the hardware cannot afford the sharpness.
     *
     * It exists for 4K TVs and projectors. Their graphics chips are chosen to decode and composite
     * video, not to shade a UI, and are far slower than a phone's; at 4K every pass the console
     * draws — the mesh backdrop above all — costs four times what it does at 1080p on hardware
     * that is nowhere near four times faster. A "premium" 4K box is MORE likely to want this than
     * a cheap 1080p stick, which never had the extra pixels to begin with.
     *
     * Read by [io.unom.punktfunk.console.SkiaConsoleShell], which applies it with
     * `SurfaceHolder.setFixedSize` — the compositor then scales the smaller buffer up for free —
     * and by the console shell itself, which draws its backdrop through a small retained pass.
     * The stream is untouched; that has its own `renderScale`.
     *
     * The data-class default is the PHONE answer. [SettingsStore.load] seeds the fold with the
     * TV answer so an absent preference resolves on there; a stored value — either way — always
     * wins, which is what keeps the user's revert honoured.
     */
    val reduceUiResolution: Boolean = false,
    /**
     * When [gamepadUiEnabled] actually takes over — the cross-client `gamepad_ui_mode` pair,
     * mirroring the Apple client's `gamepadUIMode`: `"connected"` (default, and what the switch
     * has always meant) waits for a controller; `"always"` keeps the console UI with no pad in
     * reach, for a phone or tablet that lives docked to a TV. Read only while [gamepadUiEnabled]
     * is on, which is why both settings screens hide the row when the switch is off. Anything
     * unrecognized resolves to `"connected"`. A TV ignores it — it is always in console mode.
     */
    val gamepadUiMode: String = GAMEPAD_UI_WHEN_CONNECTED,
    /**
     * The console's backdrop palette — the cross-client `ui_palette` key, named by the table in
     * `pf-console-ui`'s `theme.rs`. A device preference, never part of a preset. An unknown value
     * reads as the default: a newer client may have shipped a palette this build doesn't know.
     */
    val uiPalette: String = "violet",
    /**
     * "Low-latency mode" — the master switch over the fast pipeline: decoder ranking + per-SoC
     * vendor keys, slice-progressive delivery, pipeline thread boosts + ADPF max-performance,
     * game-tagged AAudio, DSCP marking on the media sockets, HDMI ALLM, and the forced TV mode
     * switch. (The Wi-Fi locks are NOT part of this — both are always held while streaming; see
     * StreamScreen.) Off keeps the same decode loop and presenter — so [presentPriority] applies
     * either way — with plain keys and no boosts: the per-device escape hatch.
     */
    val lowLatencyMode: Boolean = true,
    /**
     * The timeline presenter's intent — the cross-client `present_priority` pair (the Apple
     * client's "Prioritize" picker, same stored values): `"latency"` (default) = newest-wins,
     * a frame reaches glass the instant the glass budget opens; `"smooth"` = a small FIFO
     * drained one frame per vsync, absorbing network/decode jitter at one refresh of added
     * display latency per buffered frame. Anything unrecognized resolves to latency.
     */
    val presentPriority: String = "latency",
    /**
     * The smoothness buffer depth (`smooth_buffer`): 0 = Automatic (2 frames), else 1..3.
     * Only meaningful when [presentPriority] is `"smooth"`.
     */
    val smoothBuffer: Int = 0,
    /**
     * Wake-on-LAN a saved host before connecting when it isn't currently seen on mDNS. On (default):
     * a connect to a host with a learned MAC that isn't advertising sends a magic packet and waits
     * for it to reappear (see [WakeController]) before dialing. Off: always dial straight through,
     * skipping the mDNS-presence check entirely — for a host that's actually up but not visible on
     * mDNS (a flaky discovery path, a VLAN/subnet that blocks multicast, etc.), where auto-wake would
     * otherwise misfire and wait out its timeout despite the host already being reachable.
     */
    val autoWakeEnabled: Boolean = true,
    /**
     * Keep a streaming session ALIVE when the app leaves the screen. Off by default, which is
     * today's behaviour: backgrounding ends the session (non-deliberately, so the host lingers the
     * display and coming straight back reconnects fast).
     *
     * On, the session holds instead: host audio keeps playing behind an ongoing notification,
     * video decode drops with the Surface it drew into, and [backgroundTimeoutMinutes] bounds the
     * stay. The notification is what makes any of it possible — see [StreamKeepAliveService].
     */
    val backgroundKeepAlive: Boolean = false,
    /**
     * Minutes a backgrounded session runs before it disconnects itself — a battery, heat and
     * bandwidth backstop, since a host cannot tell a player who walked away from one who is
     * watching. The auto-disconnect is non-deliberate, so a late return still reconnects fast.
     * Only read when [backgroundKeepAlive] is on; the UI offers 1/5/10/30.
     */
    val backgroundTimeoutMinutes: Int = 10,
    /** Off, the host's rumble never reaches a pad or this phone's motor. Read once per session. */
    val padRumble: Boolean = true,
    /**
     * Opt-in: ALSO play the rumble the host addresses to controller 1 (wire pad 0) on this
     * phone's own vibration motor — for clip-on gamepads that ship without rumble motors, where
     * the phone body is the only actuator in the player's hands. Off by default; read once per
     * session by StreamScreen. A motorless built-in pad rumbles through the body either way. The
     * toggle is hidden on devices without a vibrator (TVs), where this would be a silent no-op.
     */
    val rumbleOnPhone: Boolean = false,
    /**
     * Opt-in: use this phone's own gyroscope as controller 1's motion when the forwarded pad has
     * none of its own — for clip-on gamepads without an IMU, where the phone body moves with the
     * player's hands. The rumble mirror's sibling, data flowing the other way. Off by default;
     * read once per session by StreamScreen (it starts a [io.unom.punktfunk.kit.DeviceGyro] only
     * when set), and the mirror stands down by itself whenever wire pad 0 is fed by a capture
     * link (USB DualSense / SC2 — pads with a real gyro). The toggle is hidden on devices
     * without a gyroscope (TVs), where this would be a silent no-op.
     */
    val gyroOnPhone: Boolean = false,

    /**
     * Capture a USB-connected Sony controller (DualSense / DualSense Edge / DualShock 4) and
     * drive it directly: the app claims the pad's HID interface and renders the host's feedback
     * by writing USB output reports — rumble works on every phone (no kernel force-feedback
     * driver needed), and adaptive triggers + lightbar + player LEDs work at all (Android has no
     * platform API for any of them). ON by default — it engages only when such a pad is attached
     * over USB at stream start; uncaptured (toggle off / no permission / Bluetooth) the pad stays
     * on the ordinary InputDevice path. USB only: Android exposes no raw path to a Bluetooth
     * Classic pad, which is also why Sony's own Remote Play has no Android trigger support.
     */
    val dsCapture: Boolean = true,

    /**
     * Render the host's DualSense **voice-coil haptics** on a captured USB pad (tier A).
     *
     * The pad's own 4-channel audio device carries them, driven directly over usbfs — Android's
     * audio framework denylists that device by VID/PID, so there is no supported route to it. The
     * two kinds are arbitrated rather than mixed, and on evidence: wire rumble is suppressed only
     * while haptics frames are actually arriving, so a title that drives classic rumble and sends
     * no haptics audio keeps rumbling. Off, or on an uncaptured/Bluetooth pad, the pad stays on
     * ordinary rumble (tier C), which on this client already drives the same actuators.
     */
    val padHaptics: Boolean = true,

    /**
     * Render the pad's **built-in speaker** on a captured USB pad. Independent of [padHaptics] —
     * the host sends the two as separate streams and either can play alone. Off by default: the
     * speaker is a small, easily-startling loudspeaker in the user's hands, and unlike haptics it
     * duplicates audio they are already hearing.
     */
    val padSpeaker: Boolean = false,

    /**
     * How a physical mouse drives the host — the cross-client mouse model (see [MouseMode]).
     * [MouseMode.DESKTOP] (default here) points absolutely; [MouseMode.CAPTURE] locks the pointer
     * to the stream ([android.view.View.requestPointerCapture]) and forwards raw relative motion.
     * Read once per session by StreamScreen; Ctrl+Alt+Shift+Q flips the capture live either way.
     */
    val mouseMode: MouseMode = MouseMode.DESKTOP,

    /**
     * Flip scroll direction — the mouse wheel and the two-finger touch scroll both. Parity with
     * the Apple/GTK clients' "Invert scroll direction".
     */
    val invertScroll: Boolean = false,
    /**
     * The in-stream quick-action ring, one JSON blob parsed by [OverlayConfig.parse] (six slots,
     * shortcuts, the virtual pad's preset). Empty = the platform default ring.
     */
    val overlayActions: String = "",
    /**
     * Back mid-stream opens the ring. Off, Back does nothing while a pad, the twist or a keyboard
     * chord can open it instead; with none of those it still opens, so a session keeps a way out.
     */
    val backOpensRing: Boolean = true,
    /**
     * Where a bare launch opens — the cross-client `start_in` key: `"hosts"` (the default),
     * `"library"` or `"stream"`. Empty or unknown reads as hosts, and with no default host every
     * value degrades to the host list. Resolve through [io.unom.punktfunk.kit.link.StartScreen],
     * never by reading this alone.
     */
    val startIn: String = "",
    /**
     * The host a bare launch opens on — a [io.unom.punktfunk.kit.security.KnownHost.id], `null`
     * when none is written. Only half the answer: with exactly one paired host saved, that host
     * is the default with nothing here, and a dangling id falls through to that same rule.
     * The cross-client `default_host` key.
     */
    val defaultHost: String? = null,
    // NOTE: clipboard sync is NOT here. It is a decision about a HOST, not about this device or
    // this stream (design/client-settings-profiles.md §3, tier H), so it lives on the host record
    // — see `KnownHost.clipboardSync`. It used to be a global here; `KnownHostStore.migrate`
    // copied that value onto every saved host and retired the key.
)

/** [Settings.touchMode] values; persisted by name. */
enum class TouchMode { TRACKPAD, POINTER, TOUCH, OFF }

/**
 * How a physical mouse drives the host — the cross-client mouse model (the Rust `MouseMode`,
 * persisted as the same lowercase names). Only meaningful with a mouse attached.
 * - [CAPTURE] — pointer lock: relative deltas, the local cursor hidden, the host's cursor the only
 *   one you see. The game model, and the desktop clients' default.
 * - [DESKTOP] — uncaptured absolute pointing: the cursor enters and leaves the stream freely. The
 *   remote-desktop model, and Android's default (a phone/TV is far more often driven by touch or a
 *   pad than by a locked mouse, and this is what the platform did before the setting existed).
 */
enum class MouseMode(val storedName: String, val label: String) {
    CAPTURE("capture", "Capture (games)"),
    DESKTOP("desktop", "Desktop (absolute)"),
}

/**
 * Stats-overlay detail tiers, in cycling order (persisted by name). Each tier is a strict superset
 * of the previous one, so toning down never hides a number a lower tier keeps:
 * - [OFF] — no overlay (and native sampling is gated off, one atomic load per frame).
 * - [COMPACT] — one line: `fps · end-to-end ms · Mb/s` (+ a loss flag when frames drop).
 * - [NORMAL] — adds the resolution/refresh line, the end-to-end p50/p95 headline, and `lost`
 *   when nonzero. The default.
 * - [DETAILED] — the full HUD: also the decoder/feed line, the stage equation, the audio
 *   latency, and the pipeline counters (skipped / FEC / judder) that read as faults at NORMAL.
 * A 3-finger tap in-stream cycles Off → Compact → Normal → Detailed → Off (see [next]).
 */
enum class StatsVerbosity(val label: String) {
    OFF("Off"),
    COMPACT("Compact"),
    NORMAL("Normal"),
    DETAILED("Detailed");

    /** The next tier for the live 3-finger-tap cycle (wraps Detailed → Off). */
    fun next(): StatsVerbosity = entries[(ordinal + 1) % entries.size]
}

/** Loads/saves [Settings] in the app-private `punktfunk_settings` prefs. */
class SettingsStore(
    context: Context,
    /**
     * A TV is the device the reduced interface is FOR (see [Settings.reduceUiResolution]). Seeded
     * into the fold's base rather than written: `Field.load` reads the seed as the absent-key
     * default, so a stored value still wins and nothing is persisted the user never picked.
     */
    private val tv: Boolean = isTvDevice(context),
) {
    private val prefs =
        context.applicationContext.getSharedPreferences("punktfunk_settings", Context.MODE_PRIVATE)

    fun load(): Settings =
        SettingsFields.ALL.fold(Settings(reduceUiResolution = tv)) { s, f -> f.load(s, prefs) }

    fun save(s: Settings) {
        val e = prefs.edit()
        SettingsFields.ALL.forEach { it.save(e, s) }
        e.apply()
    }
}

/**
 * The display the picture goes to, for capability/mode queries: the second screen the player
 * swapped it onto ([pictureDisplay]), else the context's own display, else the DEFAULT display.
 * A `punktfunk://` deep-link COLD start can reach the connect before the activity is attached to
 * its display — `context.display` then throws, and without the DEFAULT fallback the session would
 * open at 1080p60 with no HDR and nothing in the log.
 */
/** The display a stream resolves against: the picture's screen while [secondScreen] lets a pair swap. */
private fun probeDisplay(context: Context, secondScreen: Boolean = true): Display? =
    (
        runCatching { context.display }.getOrNull()
            ?: runCatching {
                context.getSystemService(DisplayManager::class.java)
                    ?.getDisplay(Display.DEFAULT_DISPLAY)
            }.getOrNull().also {
                if (it != null) Log.i("punktfunk", "display probe: context unattached — using DEFAULT_DISPLAY")
            }
        )?.let { if (secondScreen) pictureDisplay(context, it) else it }

/**
 * The device's native display mode as a landscape `(width, height, hz)` — the long edge is the
 * width, since we stream a desktop. Falls back to 1920×1080@60 if no display can be read at all
 * (see [probeDisplay] for the cold-start fallback that makes that a last resort).
 */
fun nativeDisplayMode(
    context: Context,
    spanned: Boolean = pictureSpanned(context),
    secondScreen: Boolean = true,
): Triple<Int, Int, Int> {
    val display = probeDisplay(context, secondScreen) ?: return Triple(1920, 1080, 60)
    val mode = display.mode
    val w = mode.physicalWidth
    val h = mode.physicalHeight
    // ROUNDED, not truncated: TVs report the fractional NTSC rates over HDMI (59.94, 29.97,
    // 23.976), and `toInt()` turns 59.94 into 59 — a rate no display mode anywhere has, which the
    // host then serves by clamping DOWN to the highest mode it advertises at or below it. Rounding
    // also keeps this agreeing with `Display.streamPanelFps`, which already rounds; the two
    // describe the same panel and must not disagree.
    val hz = kotlin.math.round(mode.refreshRate).toInt().coerceAtLeast(1)
    // A picture spanning two screens is two equal halves at this screen's size
    // (design/android-dual-screen.md §3.2); the codec ceiling clamps the ask later.
    return Triple(maxOf(w, h), minOf(w, h) * (if (spanned && secondScreen) 2 else 1), hz)
}

/**
 * The Aspect row's Screen and Safe area sizes, in the panel's own pixels
 * ([Resolutions.panelScreens]). A phone set to a lower resolution renders below its panel, and
 * [nativeDisplayMode] reads what it renders.
 */
fun panelScreens(context: Context): Pair<Pair<Int, Int>, Pair<Int, Int>> {
    val (w, h, _) = nativeDisplayMode(context)
    val (sw, sh, _) = safeDisplayMode(context)
    val modes = probeDisplay(context)?.supportedModes.orEmpty().map { it.physicalWidth to it.physicalHeight }
    return Resolutions.panelScreens(w to h, sw to sh, modes)
}

/**
 * Sentinel [Settings.width]/[Settings.height] meaning "the native mode, narrowed so the picture
 * clears the display cutout" — resolved at connect by [safeDisplayMode], exactly as `0` is resolved
 * by [nativeDisplayMode]. Negative, so it can never collide with a real size; distinct from the
 * UI's `-1` "Custom…" sentinel.
 */
const val SAFE_AREA_MODE = -2

/**
 * Safe-area stream geometry — the pure part, so it is unit-testable without a Display.
 *
 * The phone clips the picture in HARDWARE: the cutout (notch / punch-hole) and the four rounded
 * corners eat whatever the stream draws under them, and the stream screen draws edge-to-edge
 * (`LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS`). Which pixels survive is decided by the mode's aspect:
 *
 *  * A 16:9 mode on a 20:9 phone pillarboxes, and those bars land on the unsafe regions — which is
 *    why the fixed presets have always "just worked", at 20 % of the width.
 *  * The NATIVE mode has the panel's own aspect, so it fills every pixel, housing included.
 *
 * Asking the host for a mode narrower by the unsafe insets is the fix, and the stream screen places
 * the picture between the window's live cutout insets rather than centred: a hole on one side must
 * not be paid for on both. Pointer mapping follows for free — the input lanes derive the picture
 * rect from the live placement.
 */
object SafeArea {
    /** The host rejects odd dimensions and anything under 320 px wide (`validate_dimensions`). */
    const val MIN_WIDTH = 320

    /**
     * The width a landscape stream loses to the housing, from cutout insets read in either rotation.
     * Portrait's top and bottom become landscape's two sides, so the sum does not depend on when the
     * probe ran. The larger pair wins, which also covers a probe that raced a rotation.
     */
    fun landscapeInset(left: Int, top: Int, right: Int, bottom: Int): Int = maxOf(
        left.coerceAtLeast(0) + right.coerceAtLeast(0),
        top.coerceAtLeast(0) + bottom.coerceAtLeast(0),
    )

    /**
     * [nativeWidth] less [inset], even-floored and clamped to the host's floor. Height is untouched —
     * under aspect-fit only the horizontal axis binds on a landscape phone, so insetting it would
     * shrink the picture uncovering nothing.
     */
    fun insetWidth(nativeWidth: Int, inset: Int): Int =
        (nativeWidth - inset.coerceAtLeast(0)).coerceAtLeast(MIN_WIDTH) / 2 * 2
}

/**
 * The housing a landscape stream must clear on the picture's display, in pixels
 * ([SafeArea.landscapeInset]).
 *
 * On the activity's own display, read from the window manager's inset state, the one the stream
 * window is laid out against, so the mode and the placement agree: [Display.getCutout] adjusts for
 * the calling context's rotation and can read wider than the window does. A second screen the
 * picture was swapped onto has no window here to ask, so its cutout comes from the display. Rounded
 * corners are not charged: clearing a corner of radius `r` costs `r` on every row to uncover two
 * small arcs.
 */
fun displayCutoutInset(context: Context): Int {
    val own = ContextCompat.getDisplayOrDefault(context).displayId
    val swapped = probeDisplay(context)?.let { it.displayId != own } == true
    if (!swapped && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
        val i = runCatching {
            context.getSystemService(WindowManager::class.java).currentWindowMetrics.windowInsets
                .getInsets(WindowInsets.Type.displayCutout())
        }.getOrNull() ?: return 0
        return SafeArea.landscapeInset(i.left, i.top, i.right, i.bottom)
    }
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return 0
    val c = probeDisplay(context)?.cutout ?: return 0
    return SafeArea.landscapeInset(c.safeInsetLeft, c.safeInsetTop, c.safeInsetRight, c.safeInsetBottom)
}

/**
 * The native mode narrowed to clear this display's housing — the [SAFE_AREA_MODE] resolution, as a
 * landscape `(width, height, hz)`. Same height and refresh as [nativeDisplayMode]; only the width
 * moves, and the stream screen places the narrower picture at the window's left cutout inset.
 */
fun safeDisplayMode(
    context: Context,
    spanned: Boolean = pictureSpanned(context),
    secondScreen: Boolean = true,
): Triple<Int, Int, Int> {
    val (w, h, hz) = nativeDisplayMode(context, spanned, secondScreen)
    return Triple(SafeArea.insetWidth(w, displayCutoutInset(context)), h, hz)
}

/**
 * True when this device's display can actually present HDR10, so we should advertise HDR to the
 * host. On an SDR panel we advertise `0` instead — the host then sends a proper 8-bit BT.709 stream
 * rather than BT.2020 PQ the panel would mis-tone-map (the washed-out/dark failure). Mirrors the
 * capability gate the Apple/Windows clients apply.
 */
fun displaySupportsHdr(context: Context): Boolean {
    val display = probeDisplay(context)
    if (display == null) {
        // Distinguishable from a real SDR verdict — a silent `false` here cost an HDR session.
        Log.w("punktfunk", "display HDR probe: no display reachable — advertising SDR")
        return false
    }
    val types = buildSet {
        // API 34+: the sanctioned per-mode query (Display.Mode.getSupportedHdrTypes). The
        // deprecated Display-level hdrCapabilities can return EMPTY on Android 14+ devices
        // (Pixel-class panels included), which would make a genuinely HDR display advertise
        // no-HDR and pin the whole session to 8-bit SDR.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            display.mode.supportedHdrTypes.forEach { add(it) }
        }
        // Union the legacy query defensively — the supported one on minSdk 31, and some vendors
        // populate only this on newer APIs.
        @Suppress("DEPRECATION")
        display.hdrCapabilities?.supportedHdrTypes?.forEach { add(it) }
    }
    // HDR10/HDR10+ only: the stream is BT.2020 PQ — a Dolby-Vision/HLG-only panel can't present it.
    val supported = types.any {
        it == Display.HdrCapabilities.HDR_TYPE_HDR10 || it == Display.HdrCapabilities.HDR_TYPE_HDR10_PLUS
    }
    Log.i("punktfunk", "display HDR types=$types → advertise HDR10=$supported")
    return supported
}

/**
 * Resolve [Settings] (with its `0`=native and [SAFE_AREA_MODE] placeholders) to the concrete mode to
 * request. The safe-area sentinel is checked first because it resolves BOTH axes together — it is one
 * mode, not an independent width and height, and mixing half of it with a native height would ask
 * for a size neither sentinel means. [spanned] is the pair's kept layout unless the stream knows
 * a live one; with [secondScreen] off the pair does not exist and this screen's mode stands.
 */
fun Settings.effectiveMode(context: Context, spanned: Boolean = pictureSpanned(context)): Triple<Int, Int, Int> {
    val base = if (width == SAFE_AREA_MODE && height == SAFE_AREA_MODE) {
        safeDisplayMode(context, spanned, secondScreen)
    } else {
        nativeDisplayMode(context, spanned, secondScreen)
    }
    val w = if (width > 0) width else base.first
    val h = if (height > 0) height else base.second
    val hz = if (hz > 0) hz else base.third
    return Triple(w, h, hz)
}

/**
 * Client-side render-scale geometry — the Kotlin twin of `punktfunk-core`'s `render_scale` module
 * (and the Apple client's `RenderScale`). Multiply a base size, preserve aspect, even-floor (the
 * host rejects odd sizes), and clamp uniformly to the codec's per-axis ceiling so a connect can't
 * ask for a size the encoder rejects. `1.0` = Native. Pure + covered by [RenderScaleTest].
 */
object RenderScale {
    val PRESETS = listOf(0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0)

    /** H.264 tops out at 4096 px/axis; HEVC/AV1/auto at 8192 — the host's `codec.rs` walls. */
    fun maxDimension(codec: String): Int = if (codec == "h264") 4096 else 8192

    /** Clamp a raw multiplier into [0.5, 4.0]; a missing / non-positive / NaN value → 1.0. */
    fun sanitize(raw: Double): Double = if (raw > 0.0) raw.coerceIn(0.5, 4.0) else 1.0

    /** "Native (1×)" / "1.5×" / "2× · supersample" — the picker label. */
    fun label(scale: Double): String = when {
        scale == 1.0 -> "Native (1×)"
        scale > 1.0 -> "${trim(scale)}× · supersample"
        else -> "${trim(scale)}×"
    }

    private fun trim(s: Double): String =
        if (s == s.toLong().toDouble()) s.toLong().toString() else s.toString()

    /** Apply [scale] to a base size → a host-valid even, aspect-preserved, codec-clamped (w, h). */
    fun apply(baseW: Int, baseH: Int, scale: Double, maxDim: Int): Pair<Int, Int> {
        val s = sanitize(scale)
        var w = maxOf(baseW, 1) * s
        var h = maxOf(baseH, 1) * s
        val cap = maxDim.toDouble()
        val over = maxOf(w / cap, h / cap)
        if (over > 1.0) {
            w /= over
            h /= over
        }
        return Pair(evenFloor(w, 320), evenFloor(h, 200))
    }

    private fun evenFloor(value: Double, minimum: Int): Int {
        val v = maxOf(kotlin.math.floor(value).toInt(), minimum).coerceAtLeast(0)
        return v / 2 * 2
    }
}

/** (scale, label) for the render-scale picker. `1.0` = Native. */
val RENDER_SCALE_OPTIONS = RenderScale.PRESETS.map { it to RenderScale.label(it) }

/** [Settings.videoFit] values and labels, the desktop and console wording. */
val VIDEO_FIT_OPTIONS = listOf("fit" to "Fit", "crop" to "Crop to fill", "stretch" to "Stretch to fill")

// ---- UI option tables (value, label). The first entry is always the "auto/native" default. ----

/**
 * Stream-mode presets grouped by aspect ratio — the Kotlin twin of `punktfunk_core::resolutions`
 * (and `PunktfunkShared/Resolutions.swift`). A picker shows one family at a time behind an aspect
 * switch, plus its own native rows; picking a family moves to its size nearest the current height
 * ([nearest]), so the switch and the list always agree without picker-side state. Pure + covered
 * by [ResolutionsTest].
 */
object Resolutions {
    /** One family: the switch label, width over height, and its common panels ascending (all
     * sides even — the host rejects odd modes). */
    class Aspect(val label: String, val shape: Double, val sizes: List<Pair<Int, Int>>)

    /** Families in the order the switch shows them: most common first. */
    val ASPECTS = listOf(
        Aspect("16:9", 16.0 / 9, listOf(1280 to 720, 1920 to 1080, 2560 to 1440, 3840 to 2160, 5120 to 2880)),
        Aspect("16:10", 16.0 / 10, listOf(1280 to 800, 1920 to 1200, 2560 to 1600, 2880 to 1800, 3840 to 2400)),
        Aspect("21:9", 21.0 / 9, listOf(2560 to 1080, 3440 to 1440, 3840 to 1600, 5120 to 2160)),
        Aspect("32:9", 32.0 / 9, listOf(3840 to 1080, 5120 to 1440, 7680 to 2160)),
        Aspect("3:2", 3.0 / 2, listOf(2160 to 1440, 2256 to 1504, 2880 to 1920, 3000 to 2000)),
        Aspect("4:3", 4.0 / 3, listOf(1024 to 768, 1600 to 1200, 2048 to 1536)),
    )

    /** Shape tolerance for [aspectOf]: "21:9" panels are really 2.37–2.40, so 4 % keeps them in
     * one family and still parts 16:10 (1.60) from 3:2 (1.50). */
    private const val TOLERANCE = 0.04

    /** The family `w`×`h` belongs to by shape, not by membership: a custom 1500×1000 is 3:2.
     * `null` for a non-positive side (native, the safe-area sentinel) or a shape no family has. */
    fun aspectOf(w: Int, h: Int): Int? {
        if (w <= 0 || h <= 0) return null
        val shape = w.toDouble() / h
        return ASPECTS.indexOfFirst { kotlin.math.abs(shape / it.shape - 1) < TOLERANCE }.takeIf { it >= 0 }
    }

    /** Smallest stream mode the host accepts, per side. */
    const val MIN_WIDTH = 320
    const val MIN_HEIGHT = 200

    /** A typed `w`×`h` as a mode the host takes: each side at least [MIN_WIDTH]×[MIN_HEIGHT], at
     * most the codec's per-side ceiling, then floored even. Twin of
     * `punktfunk_core::resolutions::custom`. */
    fun custom(w: Int, h: Int, codec: String): Pair<Int, Int> {
        val cap = RenderScale.maxDimension(codec)
        return w.coerceIn(MIN_WIDTH, cap) / 2 * 2 to h.coerceIn(MIN_HEIGHT, cap) / 2 * 2
    }

    /** The size in family [aspect] nearest in height to [h]; native (`0` or a sentinel) looks for
     * 1080. Ties go to the smaller size. */
    fun nearest(aspect: Int, h: Int): Pair<Int, Int> = nearestIn(ASPECTS[aspect], h)

    /** The size in [family] nearest in height to [h]; native looks for 1080. */
    fun nearestIn(family: Aspect, h: Int): Pair<Int, Int> {
        val want = if (h <= 0) 1080 else h
        return family.sizes.minBy { kotlin.math.abs(it.second - want) }
    }

    const val SCREEN = "Screen"
    const val SAFE_AREA = "Safe area"

    /** Heights a device entry offers below the screen's own. */
    private val DEVICE_HEIGHTS = listOf(720, 1080, 1440, 2160)

    /** A device entry's sizes sit within this of its shape, tight enough to part a phone's
     * screen from its safe area. */
    private const val DEVICE_TOLERANCE = 0.01

    /**
     * The aspect switch on this device: "Screen", then "Safe area", then [ASPECTS]. Each device
     * entry appears only when no standard family has its shape, and the safe area only when it
     * differs from the screen. Twin of `punktfunk_core::resolutions::families`.
     */
    fun families(screen: Pair<Int, Int>?, safe: Pair<Int, Int>?): List<Aspect> {
        val own = listOf(SCREEN to screen, SAFE_AREA to safe).mapNotNull { (label, dims) ->
            val (w, h) = dims?.takeIf { it.first > 0 && it.second > 0 } ?: return@mapNotNull null
            if (label == SAFE_AREA && dims == screen) return@mapNotNull null
            if (aspectOf(w, h) != null) return@mapNotNull null
            val sizes = DEVICE_HEIGHTS.filter { it < h }.map { dh -> (w.toLong() * dh / h).toInt() / 2 * 2 to dh } +
                (w / 2 * 2 to h / 2 * 2)
            Aspect(label, w.toDouble() / h, sizes)
        }
        return own + ASPECTS
    }

    /** The entry of [families] `w`×`h` belongs to by shape: a device entry first, then a standard
     * one. `null` for a non-positive side or a shape none has. */
    fun familyOf(families: List<Aspect>, w: Int, h: Int): Int? {
        if (w <= 0 || h <= 0) return null
        val shape = w.toDouble() / h
        fun within(a: Aspect, tol: Double) = kotlin.math.abs(shape / a.shape - 1) < tol
        fun device(a: Aspect) = a.label == SCREEN || a.label == SAFE_AREA
        return families.indexOfFirst { device(it) && within(it, DEVICE_TOLERANCE) }.takeIf { it >= 0 }
            ?: families.indexOfFirst { !device(it) && within(it, TOLERANCE) }.takeIf { it >= 0 }
    }

    /** The panel as the largest of [modes] in the [current] mode's shape, and the [safe] area
     * scaled up to it with even sides. Both stay at [current] when no mode is larger. */
    fun panelScreens(
        current: Pair<Int, Int>,
        safe: Pair<Int, Int>,
        modes: List<Pair<Int, Int>>,
    ): Pair<Pair<Int, Int>, Pair<Int, Int>> {
        val (cw, ch) = current
        if (cw <= 0 || ch <= 0) return current to safe
        val panel = (modes.map { (a, b) -> maxOf(a, b) to minOf(a, b) } + current)
            .filter { (w, h) -> h > 0 && kotlin.math.abs(w.toDouble() * ch / (h.toDouble() * cw) - 1) < DEVICE_TOLERANCE }
            .maxBy { (w, h) -> w.toLong() * h }
        if (panel == current) return current to safe
        fun up(side: Int) = (side.toLong() * panel.second / ch).toInt() / 2 * 2
        return panel to if (safe == current) panel else up(safe.first) to up(safe.second)
    }
}

/** (width, height, label) rows every family shares: `(0,0)` = native display; [SAFE_AREA_MODE] =
 * native minus the cutout. */
val NATIVE_RESOLUTION_OPTIONS = listOf(
    Triple(0, 0, "Native display"),
    Triple(SAFE_AREA_MODE, SAFE_AREA_MODE, "Native display (safe area)"),
)

/** The Resolution picker's rows for one family: the native rows, then that family's sizes. */
fun resolutionOptions(family: Resolutions.Aspect): List<Triple<Int, Int, String>> =
    NATIVE_RESOLUTION_OPTIONS + family.sizes.map { (w, h) -> Triple(w, h, "$w × $h") }

/** The entry the Resolution picker lists for the stored size: its shape. Native and the safe-area
 * mode read as this device's own entries where it has them; otherwise, and for a shape none has,
 * the first entry. */
fun Settings.resolutionFamily(families: List<Resolutions.Aspect>): Int {
    fun own(label: String) = families.indexOfFirst { it.label == label }.takeIf { it >= 0 }
    return when {
        width == 0 -> own(Resolutions.SCREEN)
        width == SAFE_AREA_MODE -> own(Resolutions.SAFE_AREA) ?: own(Resolutions.SCREEN)
        else -> Resolutions.familyOf(families, width, height)
    } ?: 0
}

/** True when the stored size is none of the presets its family lists — a custom resolution typed
 * in the touch settings. Detected from the size itself rather than a persisted flag, so it can
 * never disagree with what's actually stored (mirrors the Apple client). */
fun Settings.isCustomResolution(families: List<Resolutions.Aspect> = Resolutions.families(null, null)): Boolean =
    resolutionOptions(families[resolutionFamily(families)]).none { (w, h, _) -> w == width && h == height }

/** (hz, label). `0` = native refresh. */
val REFRESH_OPTIONS = listOf(
    0 to "Native",
    30 to "30 Hz",
    60 to "60 Hz",
    90 to "90 Hz",
    120 to "120 Hz",
    144 to "144 Hz",
    165 to "165 Hz",
    240 to "240 Hz",
)

/** (channel count, label). 2 = stereo (default), 6 = 5.1, 8 = 7.1. */
val AUDIO_CHANNEL_OPTIONS = listOf(
    2 to "Stereo",
    6 to "5.1 Surround",
    8 to "7.1 Surround",
)

/** Opus 48 kHz — the default, and byte-for-byte the session every earlier build ran. */
const val AUDIO_FORMAT_OPUS = "opus"

/**
 * Bit-exact PCM at 44.1 kHz / 24-bit (~2.1 Mbps). The CD family's base rate: what an ordinary
 * Windows endpoint or a 44.1 kHz interface reports as its own engine rate, and the request that
 * spares such a host a resample it would otherwise do on the way out.
 */
const val AUDIO_FORMAT_LOSSLESS_441 = "lossless441"

/**
 * Bit-exact PCM at 48 kHz / 24-bit (~2.3 Mbps). The honest win even without a hi-res interface:
 * no lossy stage at all, and no double resample on a host whose engine already runs at 48 kHz.
 */
const val AUDIO_FORMAT_LOSSLESS_48 = "lossless48"

/** Bit-exact PCM at 88.2 kHz / 24-bit (~4.2 Mbps) — 96 kHz's counterpart in the 44.1 family. */
const val AUDIO_FORMAT_LOSSLESS_882 = "lossless882"

/**
 * Bit-exact PCM at 96 kHz / 24-bit (~4.6 Mbps), and only real if the host's capture endpoint
 * genuinely runs at 96 kHz — the host declines rather than upsampling to meet the request.
 */
const val AUDIO_FORMAT_LOSSLESS_96 = "lossless96"

/**
 * Bit-exact PCM at 176.4 kHz / 24-bit — **8.5 Mbps**, and the one row far more likely to be
 * declined than granted. Three separate things have to go right: the host's bandwidth gate gives
 * audio at most a quarter of the video budget, so the session needs ~34 Mbps of video before it
 * will even consider it; a stereo frame only fits a QUIC datagram on the ladder's shortest rung
 * (1 ms — a thousand datagrams a second — at ~1 069 B, so the first connection with a smaller
 * datagram declines it), and a surround one fits no rung at all; and very few Android outputs will
 * open the rate, which the native probe settles before the handshake. Offered because it is
 * reachable, not because it is likely — the HUD's `audio lossless …` line is what says which
 * happened.
 */
const val AUDIO_FORMAT_LOSSLESS_1764 = "lossless1764"

/**
 * (stored value, label) for the requested audio format — the cross-client table, matching the
 * Apple client's `AudioFormatChoice` raw values and the desktop `AUDIO_FORMATS` so a preset
 * written on any of them is honoured on the others.
 *
 * ⚠ **The stored values are shared VERBATIM and must never be renamed.** A preset carries the key
 * through untouched, so a spelling that differs by one character fails in the worst possible way:
 * the preset keeps "working" on the other client and silently inherits its global default
 * instead. The naming rule is the kHz figure with the decimal point dropped — `lossless48`,
 * `lossless96`, and for the 44.1 family `lossless441` / `lossless882` / `lossless1764`.
 *
 * **Both rate families are here now.** They were not: every buffer figure in the shared jitter
 * policy used to be `ms × perMs` with `perMs` an INTEGER number of samples per millisecond, which
 * made 44 100 → 44.1 truncate to 44 — a silent 2.3 % error in every target, every de-prime fuse
 * and every reported buffer depth, and the whole reason the 44.1 family was deferred rather than
 * refused (design/hi-res-audio.md §4.1). Core now multiplies before it divides, which is exact at
 * every rate, so the deferral is lifted.
 *
 * A row being offered is not a promise it can be delivered: the host's gate, this device's own
 * output, and the path MTU each get a veto, and the ones at the top of the list get vetoed often.
 * What actually happened is on the HUD.
 *
 * Lossless at **16**-bit is deliberately absent at every rate: it spends ~1.4–1.5 Mbps to sound
 * like the transparent 256 kbps Opus it replaces, and it is the one lossless request whose wire
 * parameters are indistinguishable from a legacy one. 24-bit is where the plane earns its
 * bandwidth.
 */
val AUDIO_FORMAT_OPTIONS = listOf(
    AUDIO_FORMAT_OPUS to "Standard (Opus)",
    AUDIO_FORMAT_LOSSLESS_441 to "Lossless 44.1 kHz / 24-bit",
    AUDIO_FORMAT_LOSSLESS_48 to "Lossless 48 kHz / 24-bit",
    AUDIO_FORMAT_LOSSLESS_882 to "Lossless 88.2 kHz / 24-bit",
    AUDIO_FORMAT_LOSSLESS_96 to "Lossless 96 kHz / 24-bit",
    AUDIO_FORMAT_LOSSLESS_1764 to "Lossless 176.4 kHz / 24-bit",
)

/**
 * The `(rateHz, bits)` pair [audioFormat] asks the host for, in `nativeConnect`'s terms.
 *
 * ⚠⚠ **Opus is `0`/`0`, the "did not ask" sentinel — NOT `48000`/`16`.** Core sets
 * `CLIENT_CAP_AUDIO_HIRES` when either field is non-zero, because it keys on *a format was
 * specified* rather than *the format differs from the default*: 48 kHz/16-bit is the cheapest
 * lossless rung as well as the legacy pair, so the other rule would make it the one rung nobody
 * could ask for. Sending `48000`/`16` for a user who chose Standard therefore advertises the
 * capability, and the host then hands that user 1.5 Mbps of lossless PCM instead of 256 kbps of
 * Opus. This returned that pair until all four clients were compared.
 *
 * ⚠⚠ **That bug got worse on 2026-08-17, when the host's `PUNKTFUNK_AUDIO_HIRES` gate went
 * default-ON.** It used to need a host whose operator had opted in — rare, so a slip here would
 * have been survivable and probably unnoticed. The blast radius is now every host that has not
 * deliberately opted out, i.e. all of them. The zeroes below are load-bearing.
 *
 * The zeroes are also what keeps a default `Hello` byte-identical to a pre-lossless one — the wire
 * encodes an explicit 48 000/16 the same as absent, and the whole difference is the capability bit.
 *
 * Deriving the pair FROM the stored format is what stops the two ever disagreeing. An unrecognized
 * stored value — a newer build's, or a corrupted pref — resolves to Opus rather than blocking the
 * connect.
 *
 * The rate this returns is only the REQUEST. The native side runs it down a fallback ladder first
 * (`session::connect::rate_fallback_ladder`), because AAudio grants an explicitly-asked rate or
 * fails the open and never substitutes — so a rate this device cannot play must never reach the
 * wire.
 */
fun Settings.audioFormatWire(): Pair<Int, Int> = when (audioFormat) {
    AUDIO_FORMAT_LOSSLESS_441 -> 44_100 to 24
    AUDIO_FORMAT_LOSSLESS_48 -> 48_000 to 24
    AUDIO_FORMAT_LOSSLESS_882 -> 88_200 to 24
    AUDIO_FORMAT_LOSSLESS_96 -> 96_000 to 24
    AUDIO_FORMAT_LOSSLESS_1764 -> 176_400 to 24
    else -> AUDIO_FORMAT_WIRE_UNSPECIFIED
}

/**
 * The `(rateHz, bits)` that mean "this session is not asking for the lossless plane" — see
 * [audioFormatWire] for why it is a pair of zeroes rather than the legacy 48 000/16.
 */
val AUDIO_FORMAT_WIRE_UNSPECIFIED = 0 to 0

/**
 * (stored value, label) for the preferred video codec — the cross-client table (the Rust
 * `CODECS`), so a value another client or a preset stored is always representable here.
 * `"auto"` = host decides.
 *
 * Two rows are capability-gated by [codecOptionsFor] rather than dropped from the table: `"av1"`
 * needs a real `video/av01` decoder on this device, and `"pyrowave"` needs a Vulkan 1.3 GPU with
 * the codec's compute feature set (`NativeBridge.nativePyrowaveCapable`) — it is the one codec
 * here that is not a MediaCodec at all.
 */
val CODEC_OPTIONS = listOf(
    "auto" to "Automatic",
    "hevc" to "HEVC (H.265)",
    "h264" to "H.264 (AVC)",
    "av1" to "AV1",
    "pyrowave" to "PyroWave (wired LAN)",
)

/**
 * [CODEC_OPTIONS] minus the rows this device can't decode — a preference the client never
 * advertises is a setting that does nothing. [stored] is the currently persisted value, which is
 * always kept selectable so the selection can be rendered (the don't-clobber rule: a codec chosen
 * on another device, or by a newer build, must survive being looked at here).
 *
 * A kept row says why it is not in effect. Reading plain "AV1" while every session streams HEVC
 * is the whole of #1138: the picker is the only place that can say the Hello never asked for it.
 */
fun codecOptionsFor(
    stored: String,
    av1Capable: Boolean,
    pyrowaveCapable: Boolean,
): List<Pair<String, String>> =
    CODEC_OPTIONS.mapNotNull { (v, label) ->
        val capable = when (v) {
            "av1" -> av1Capable
            "pyrowave" -> pyrowaveCapable
            else -> true
        }
        when {
            capable -> v to label
            stored != v -> null
            v == "av1" -> v to "AV1 — no hardware decoder here; HEVC is used"
            else -> v to "PyroWave — this GPU can't decode it; HEVC is used"
        }
    }

/** Resolved [Settings.systemButtons]: forward the raw guide/misc presses? Auto = forward on
 * Android — the press reaches the app on most devices, and where the shell shows its own UI
 * for it that's the shell's business. */
fun Settings.systemButtonsForward(): Boolean = systemButtons != "local"

/** Resolved [Settings.guideGesture]: auto = OFF on Android (the raw press already reaches the
 * host); "on" is for devices whose shell intercepts the physical guide button. */
fun Settings.guideGestureEnabled(): Boolean = guideGesture == "on"

/** The [Settings.codec] string as a `quic::CODEC_*` preference byte (`0` = auto). H264=1, HEVC=2,
 * AV1=4, PyroWave=8 — the shared cross-client contract. */
fun Settings.preferredCodec(): Int = when (codec) {
    "h264" -> 1
    "hevc" -> 2
    "av1" -> 4
    "pyrowave" -> 8
    else -> 0
}

/** (kbps, label). `0` = host default. */
val BITRATE_OPTIONS = listOf(
    0 to "Automatic",
    10_000 to "10 Mbps",
    20_000 to "20 Mbps",
    50_000 to "50 Mbps",
    100_000 to "100 Mbps",
    150_000 to "150 Mbps",
    200_000 to "200 Mbps",
    300_000 to "300 Mbps",
    500_000 to "500 Mbps",
)

/** The Bitrate menu's "Custom…" entry. Never stored: picking it opens the Mbps field. */
const val CUSTOM_BITRATE = -1

/** The typed field's ceiling, the console shell's on Android: 2 Gbps. The host takes up to 8. */
const val CUSTOM_BITRATE_MAX_MBPS = 2_000

/** True when the stored rate is none of [BITRATE_OPTIONS] — typed, or written by the speed test. */
fun Settings.isCustomBitrate(): Boolean = BITRATE_OPTIONS.none { it.first == bitrateKbps }

/** "14 Mbps" or "14.9 Mbps": any stored rate, so an off-menu one never reads as Automatic. */
fun bitrateLabel(kbps: Int): String =
    if (kbps % 1000 == 0) "${kbps / 1000} Mbps" else "%.1f Mbps".format(kbps / 1000.0)

/** (CompositorPref wire byte, label). Byte 6, a Windows host's echo, is never a choice. */
val COMPOSITOR_OPTIONS = listOf(
    0 to "Automatic",
    1 to "KWin (KDE Plasma)",
    3 to "Mutter (GNOME)",
    5 to "Hyprland",
    2 to "wlroots (Sway / River)",
    4 to "gamescope",
)

/** (verbosity, label) for the stats-overlay detail picker. Order = the live 3-finger-tap cycle. */
val STATS_VERBOSITY_OPTIONS = StatsVerbosity.entries.map { it to it.label }

/** (cross-client `hud_placement` name, label) — core's `HudCorner`. */
val HUD_PLACEMENT_OPTIONS = listOf(
    "topLeading" to "Top left",
    "topTrailing" to "Top right",
    "bottomLeading" to "Bottom left",
    "bottomTrailing" to "Bottom right",
)

/** The overlay's size in percent — core's `STATS_SCALE_PCTS`. */
val STATS_SCALE_OPTIONS = listOf(75, 100, 125, 150, 175, 200).map { it to "$it %" }

/** [Settings.presentPriority] as the wire int `nativeStartVideo` takes (0 = latency, 1 = smooth).
 * Unrecognized values resolve to latency — same rule as the Apple client. */
fun Settings.presentPriorityWire(): Int = if (presentPriority == "smooth") 1 else 0

/** (stored value, label) for the presenter-intent picker — the Apple client's table verbatim. */
val PRESENT_PRIORITY_OPTIONS = listOf(
    "latency" to "Lowest latency",
    "smooth" to "Smoothness",
)

/** (minutes, label) for the background keep-alive's give-up timer — the Apple client's table. */
val BACKGROUND_TIMEOUT_OPTIONS = listOf(
    1 to "1 minute",
    5 to "5 minutes",
    10 to "10 minutes",
    30 to "30 minutes",
)

/** (frames, label) for the smoothness-buffer picker; each buffered frame ≈ one refresh interval
 * of jitter absorbed for one interval of added display latency ([hz] labels the cost). */
fun smoothBufferOptions(hz: Int): List<Pair<Int, String>> {
    val periodMs = 1000.0 / maxOf(24, hz)
    fun cost(frames: Int) = "+%.0f ms".format(periodMs * frames)
    return listOf(
        0 to "Automatic",
        1 to "1 frame (${cost(1)})",
        2 to "2 frames (${cost(2)})",
        3 to "3 frames (${cost(3)})",
    )
}

/** (stored value, label) for when the console UI takes over — the Apple client's table verbatim.
 * Only offered while [Settings.gamepadUiEnabled] is on; a TV is in console mode either way. */
val GAMEPAD_UI_MODE_OPTIONS = listOf(
    GAMEPAD_UI_WHEN_CONNECTED to "With a controller",
    GAMEPAD_UI_ALWAYS to "Always",
)

/** (stored value, label) for where a bare launch opens — the cross-client table verbatim. */
val START_IN_OPTIONS = io.unom.punktfunk.kit.link.StartIn.entries.map { it.stored to it.label }

/** (mode, label) for the touch-input model. */
val TOUCH_MODE_OPTIONS = listOf(
    TouchMode.TRACKPAD to "Trackpad",
    TouchMode.POINTER to "Direct pointer",
    TouchMode.TOUCH to "Touch passthrough",
    TouchMode.OFF to "Off",
)

/** (mode, label) for the physical-mouse model. */
val MOUSE_MODE_OPTIONS = MouseMode.entries.map { it to it.label }

/**
 * (GamepadPref wire byte, label) for the emulated pad the host creates. NOT positional: the wire
 * bytes are `punktfunk_core::config::GamepadPref` (see `Gamepad.PREF_*`), and Steam Deck is `6`
 * with `5` (the classic Steam Controller) deliberately not offered — the same subset the desktop
 * clients' picker shows.
 */
val GAMEPAD_OPTIONS = listOf(
    io.unom.punktfunk.kit.Gamepad.PREF_AUTO,
    io.unom.punktfunk.kit.Gamepad.PREF_XBOX360,
    io.unom.punktfunk.kit.Gamepad.PREF_DUALSENSE,
    io.unom.punktfunk.kit.Gamepad.PREF_XBOXONE,
    io.unom.punktfunk.kit.Gamepad.PREF_DUALSHOCK4,
    io.unom.punktfunk.kit.Gamepad.PREF_STEAMDECK,
).map { it to io.unom.punktfunk.kit.Gamepad.prefLabel(it) }

/** (stored `system_buttons` value, label) — where the guide/share presses land while streaming. */
val SYSTEM_BUTTON_OPTIONS = listOf(
    "auto" to "Automatic",
    "forward" to "Send to host",
    "local" to "This device",
)

/** (stored `guide_gesture` value, label) — the hold-Select guide gesture. */
val GUIDE_GESTURE_OPTIONS = listOf(
    "auto" to "Automatic",
    "on" to "On",
    "off" to "Off",
)
