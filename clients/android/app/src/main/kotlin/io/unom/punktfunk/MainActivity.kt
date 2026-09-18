package io.unom.punktfunk

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.ViewConfiguration
import android.view.MotionEvent
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.systemBars
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import io.unom.punktfunk.kit.DsDevice
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.GamepadRouter
import io.unom.punktfunk.kit.Keymap
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.RingNav
import io.unom.punktfunk.kit.Sc2BleLink
import io.unom.punktfunk.kit.Sc2Device
import io.unom.punktfunk.kit.SessionAccess
import io.unom.punktfunk.kit.ringNavForKey
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.HostResolution
import io.unom.punktfunk.kit.security.KnownHostStore

/** Broadcast action for the menu-time SC2 USB-permission grant (see [MainActivity.startSc2MenuNav]). */
private const val SC2_MENU_PERMISSION = "io.unom.punktfunk.SC2_MENU_USB_PERMISSION"

/** Request code for the SC2's Bluetooth grant (see [MainActivity.maybeAskSc2BtPermission]). */
private const val REQ_SC2_BLUETOOTH = 0x5C2B

/** Never ours, however modal the thing on screen is: the box's own volume and power. */
private val SYSTEM_KEYS = intArrayOf(
    KeyEvent.KEYCODE_VOLUME_UP,
    KeyEvent.KEYCODE_VOLUME_DOWN,
    KeyEvent.KEYCODE_VOLUME_MUTE,
    KeyEvent.KEYCODE_POWER,
)

/**
 * Keeps ONE window-insets reader alive for as long as the app's UI exists — the fix for the menus
 * coming back from a stream laid out against the WRONG safe area.
 *
 * Compose attaches its `OnApplyWindowInsets` and `WindowInsetsAnimation` callbacks when the first
 * composable reads an inset, and removes them again when the last reader goes away
 * (`WindowInsetsHolder.increment/decrementAccessors`). [StreamScreen] reads no insets outside the
 * safe-area mode — it's a bare full-screen surface — so a stream drops the reader count to zero.
 *
 * That alone is survivable; what isn't is a session that ends while the app is BACKGROUNDED, which
 * is the common case (leaving the app ends the session — see StreamScreen's ON_STOP observer). The
 * whole window restore — `show(systemBars())`, releasing the landscape lock — then runs on a stopped
 * activity, and the corrected insets that follow arrive while Compose has no listener attached. When
 * the menus recompose, `incrementAccessors` re-attaches and asks for a fresh pass, but a stopped
 * window produces no dispatch, and on resume nothing has *changed* any more, so none ever comes.
 * Compose keeps serving what it last saw: the landscape, bars-hidden values.
 *
 * That's exactly what the reporter's phone showed (on-glass 2026-07-29, verified by dump): the
 * platform reported `bars=[0,162,0,72] cutout=[0,162,0,0]` for the window while the layout was still
 * using the landscape immersive set — cutout `left=162` (Material3 lays out against
 * `systemBars.union(displayCutout)`), bars all zero. Content shoved right by the landscape cutout,
 * nothing kept clear of the status bar or the gesture pill, and no rotation or IME animation could
 * shake it loose. A/B'd over eight runs of the real teardown sequence: 3 of 4 wrong without this,
 * 4 of 4 correct with it.
 *
 * Reading an inset here holds the count above zero for the activity's whole life, so the listeners
 * survive the stream and every dispatch lands. It subscribes to no inset VALUE (only the holder
 * object), so it triggers no recomposition — the cost is one DisposableEffect.
 */
@Composable
private fun HoldWindowInsetsListeners() {
    // The read itself IS the registration (the accessor is scoped to this composable, which never
    // leaves the composition); `remember` is only what keeps it from being a value nobody uses.
    remember(WindowInsets.systemBars) {}
}

class MainActivity : ComponentActivity() {
    /**
     * The active stream session handle (0 = not streaming). Set by [StreamScreen] while it's shown.
     * `dispatchKeyEvent` is the earliest key hook an app has — above Compose's focus system — so
     * hardware keys are forwarded regardless of which view holds focus; [KeyCaptureService] is
     * earlier still, and follows this handle and the window's focus.
     */
    var streamHandle: Long = 0L
        set(value) {
            field = value
            syncKeyCapture()
        }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        syncKeyCapture()
    }

    /** The key-filtering service forwards to this window only while a stream has focus. */
    private fun syncKeyCapture() {
        val focused = streamHandle != 0L && hasWindowFocus()
        if (!focused) cancelKeyRepeat()
        KeyCaptureService.stream = if (focused) ::streamKey else null
    }

    /**
     * Key repeat for the service path. Android synthesises repeats in the input dispatcher,
     * and a key the accessibility filter consumed never reaches it, so a held key sent one
     * DOWN. Armed only while the service is on; the dispatcher path repeats on its own.
     */
    private val keyRepeat = Handler(Looper.getMainLooper())
    private var repeatVk = 0

    private fun armKeyRepeat(handle: Long, vk: Int) {
        cancelKeyRepeat()
        repeatVk = vk
        val delay = ViewConfiguration.getKeyRepeatDelay().toLong()
        val tick = object : Runnable {
            override fun run() {
                if (streamHandle != handle || repeatVk != vk || !KeyCaptureService.running) return
                NativeBridge.nativeSendKey(handle, vk, true, 0)
                keyRepeat.postDelayed(this, delay)
            }
        }
        keyRepeat.postDelayed(tick, ViewConfiguration.getKeyRepeatTimeout().toLong())
    }

    private fun cancelKeyRepeat() {
        repeatVk = 0
        keyRepeat.removeCallbacksAndMessages(null)
    }

    /**
     * The active session's access-grant mask ([SessionAccess] bits) — set with [streamHandle] by
     * StreamScreen and kept live by its access poll; back to [SessionAccess.ALL] when the stream
     * leaves. Consulted only while streaming: the VK keyboard path below goes inert without
     * [SessionAccess.KEYBOARD] (the keys are consumed, not sent — the host would drop them, and
     * letting them fall through would drive Android navigation under a live stream). Courtesy
     * gating; the host enforces regardless.
     */
    var streamAccess: Int = SessionAccess.ALL

    /**
     * Multi-controller router for the active session (built/released by StreamScreen): assigns each
     * connected pad a stable wire index, threads it onto every event, declares/removes pads on
     * hot-plug, and routes rumble/HID feedback back by pad index. Null while not streaming.
     */
    var gamepadRouter: GamepadRouter? = null

    /**
     * One screen's claim on the pad while not streaming: its key/motion observers, consulted for
     * every event before the focus-navigation fallbacks below; a `true` return consumes the event.
     * Holders are the Skia console shell, [GamepadNavEffect2D] on the Compose screens the console
     * opens over itself, and the Controllers screen's input test.
     */
    class PadProbes(val key: (KeyEvent) -> Boolean, val motion: (MotionEvent) -> Boolean)

    /**
     * The pad-probe claims, a STACK — only the top entry sees events. A single last-writer-wins
     * slot is how the console shell used to lose the pad for good: a screen composed over it
     * (Controllers/Licenses) overwrote the slot, then nulled it on its way out, and the shell —
     * whose install effect had no reason to re-run — never got it back. Pushing on install and
     * removing BY IDENTITY on dispose survives every ordering Compose produces (cross-fades
     * compose both screens at once, and dispose is not always LIFO): whatever leaves takes only
     * its own entry, and whatever is left on top resumes seeing the pad.
     */
    private val padProbes = mutableListOf<PadProbes>()

    fun pushPadProbes(p: PadProbes) { padProbes += p }
    fun removePadProbes(p: PadProbes) { padProbes.remove(p) }

    private val padKeyProbe: ((KeyEvent) -> Boolean)? get() = padProbes.lastOrNull()?.key
    private val padMotionProbe: ((MotionEvent) -> Boolean)? get() = padProbes.lastOrNull()?.motion

    /**
     * Physical-mouse forwarder for the active session (built/released by StreamScreen, like
     * [gamepadRouter]): uncaptured hover/click/wheel forwards as absolute cursor input, captured
     * ([android.view.View.requestPointerCapture]) raw deltas as relative mouse-look. The dispatch
     * overrides below route every SOURCE_MOUSE event here while streaming. Null while not streaming.
     */
    var mouseForwarder: MouseForwarder? = null

    /**
     * The quick-action ring's claim on the keyboard/remote — non-null exactly while the ring is up
     * (set from its open/close edge, beside the pad router's own mask). A pad reaches the ring
     * through [gamepadRouter]; every other device reaches it through here, which is what makes the
     * ring drivable from a TV remote.
     */
    var ringKeys: ((RingNav) -> Unit)? = null

    /** Opens the ring at the screen centre, for Ctrl+Alt+Shift+O. Null while not streaming. */
    var openRing: (() -> Unit)? = null

    private val isTv by lazy { isTvDevice(this) }

    /**
     * TV remote-as-pointer for the active session (StreamScreen builds it on TV devices only):
     * hold SELECT to toggle, then the D-pad glides the host cursor. Consulted first for
     * non-gamepad keys while streaming. Null while not streaming or not a TV.
     */
    var remotePointer: RemotePointer? = null

    /**
     * Set by [StreamScreen] to its disconnect action. The emergency-exit chord (below) invokes it so a
     * couch user with no keyboard/Back can always leave a stream.
     */
    var requestStreamExit: (() -> Unit)? = null

    /**
     * Whether the last console input came from a real gamepad (face buttons / stick) vs. a TV D-pad
     * remote (which has no A/B/X/Y). The console UI reads this to show glyphs the user recognises — pad
     * face buttons, or a select glyph + arrows for a remote. Compose observes it (a snapshot state).
     * Defaults to the remote glyphs on a TV (its D-pad remote is the typical first input, and often the
     * only one) and to gamepad glyphs everywhere else (the console UI on a phone/tablet only activates
     * via a real controller, so a TV-remote glyph would be a wrong first impression there) — set from
     * [onCreate] once a [Context] is available, then kept live by real input.
     */
    var lastPadIsGamepad by mutableStateOf(true)
        private set

    /**
     * The glyph family of the controller driving the console UI (Xbox letters / PlayStation shapes /
     * Nintendo monochrome) — seeded from the first connected pad, then kept live by real input the
     * same way [lastPadIsGamepad] is. Compose observes it (a snapshot state); the hint bar picks its
     * button glyphs from it so a DualSense user isn't shown Xbox lettering.
     */
    var lastPadStyle by mutableStateOf(Gamepad.PadStyle.GENERIC)
        private set

    /**
     * The `InputDevice.id` of the controller driving the console UI, or 0 for none. Kept beside
     * [lastPadStyle] because the console's menu haptics render on the DRIVING pad's own motors when
     * it has any — a rumble that comes out of the device you are not holding is worse than none.
     * Falls back to the phone body (see `rememberConsoleHaptics`), and to silence on a TV, where
     * neither a remote nor the box has an actuator.
     */
    var lastPadDeviceId by mutableIntStateOf(0)
        private set

    /**
     * A `punktfunk://` URL waiting to be routed — set from the VIEW intent that started (or
     * re-entered) this activity, cleared by whoever handles it. Compose observes it.
     *
     * Read in BOTH [onCreate] and [onNewIntent] on purpose: `launchMode` is `standard`, so a second
     * link usually arrives as a fresh activity instance (onCreate) and only sometimes as a new
     * intent on this one (a caller that set `FLAG_ACTIVITY_SINGLE_TOP`). A link arriving while an
     * earlier one is still unhandled replaces it — the user's latest intent is the live one.
     */
    var pendingDeepLink by mutableStateOf<String?>(null)

    /** The panel's highest-refresh display mode (0 = unknown/unsupported), resolved once at startup. */
    private var highRefreshModeId = 0

    /**
     * Menu-time Steam Controller 2 capture (UI mode — no router): a captured SC2 never produces
     * ordinary gamepad events (lizard mode is kb/mouse; the claim removes even those), so this
     * drives the console UI directly from the parsed reports via [sc2NavKey]. Runs while the app
     * is foreground and NOT streaming; StreamScreen pauses it around its own stream-mode capture.
     * [sc2MenuActive] is observed by the console-UI gate ([rememberControllerConnected]) and the
     * Controllers screen.
     */
    private var sc2Menu: io.unom.punktfunk.kit.Sc2Capture? = null
    var sc2MenuActive by mutableStateOf(false)
        private set

    /** A wired SC2 or Puck is plugged in — the console lists it before the capture grant lands. */
    var sc2UsbAttached by mutableStateOf(false)
        private set
    private var sc2Receiver: BroadcastReceiver? = null
    private var sc2PermissionAsked = false

    /** Bluetooth asked once this process — a denial must not re-prompt on every resume. */
    private var sc2BtPermissionAsked = false

    /** Sony-pad USB grant asked this attach — a deny doesn't re-nag until a fresh attach (or the
     *  Controllers screen's explicit button). */
    private var dsPermissionAsked = false

    /**
     * Compose focus hook for the SC2's synthetic D-pad (set by [onCreate]'s composition). A
     * synthetic KeyEvent dispatched from OUTSIDE the real input pipeline never reaches
     * ViewRootImpl's focus-navigation stage — the one that grants initial focus for a real
     * pad's first D-pad press — so on a phone in touch mode it lands on a focus-less window
     * and does nothing (first on-glass run: only B worked, since it bypasses key events
     * entirely). `FocusManager.moveFocus` is the public API for exactly this.
     */
    private var sc2MoveFocus: ((androidx.compose.ui.focus.FocusDirection) -> Boolean)? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // A URL may never preempt a live session (design/client-deep-links.md §3.2). With
        // `launchMode = standard` a link normally arrives as a NEW activity instance in a new task
        // — the streaming one gets backgrounded, and backgrounding ends a session — so the refusal
        // has to happen HERE, before this instance is resumed, not inside the composition (which
        // only ever sees the rare `onNewIntent` case). Finishing now leaves the streaming task in
        // front, untouched.
        val live = SessionGate.live
        if (live != null && deepLinkFrom(intent) != null) {
            // Pointing at the host already being streamed is the one exception, and its right
            // answer is to do nothing: the intent has already brought the app forward, which is
            // what "focus it" means here.
            if (!targetsHost(intent, live)) {
                Toast.makeText(
                    this,
                    "Already streaming — end this session first.",
                    Toast.LENGTH_LONG,
                ).show()
            }
            finish()
            return
        }
        pendingDeepLink = deepLinkFrom(intent)
        lastPadIsGamepad = !isTvDevice(this)
        lastPadStyle = Gamepad.styleFor(Gamepad.firstPad())
        resolveHighRefreshMode()
        setConsoleHighRefreshRate(true) // the console UI wants max refresh; streaming manages its own
        // Dark, transparent system bars regardless of the system theme — our UI is always dark, so
        // the status/nav bars blend with our surface and get light icons. (The no-arg edge-to-edge
        // picks the *system* light/dark, which left a black status bar over our dark background.)
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
        // Dev escape hatch (mirrors the Apple client's PUNKTFUNK_FORCE_GAMEPAD_UI): force the console
        // UI without a physical pad — `adb shell am start -n io.unom.punktfunk/.MainActivity --ez
        // pf_force_gamepad_ui true`. Never set in normal use; real activation is a connected pad / TV.
        val forceGamepadUi = intent?.getBooleanExtra("pf_force_gamepad_ui", false) ?: false
        // SC2 hot-plug + the menu-time USB-permission grant both (re)start the menu capture.
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(c: Context?, intent: Intent?) {
                when (intent?.action) {
                    UsbManager.ACTION_USB_DEVICE_ATTACHED,
                    UsbManager.ACTION_USB_DEVICE_DETACHED,
                    -> {
                        sc2PermissionAsked = false // a fresh attach may ask once again
                        startSc2MenuNav()
                        dsPermissionAsked = false
                        maybeAskDsPermission()
                    }
                    SC2_MENU_PERMISSION -> {
                        if (intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)) {
                            startSc2MenuNav()
                        }
                    }
                }
            }
        }
        sc2Receiver = receiver
        val filter = IntentFilter().apply {
            addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED)
            addAction(UsbManager.ACTION_USB_DEVICE_DETACHED)
            addAction(SC2_MENU_PERMISSION)
        }
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            registerReceiver(receiver, filter)
        }
        setContent {
            PunktfunkTheme {
                HoldWindowInsetsListeners()
                // Focus hook for the SC2's synthetic navigation (see [sc2MoveFocus]). `Next` is
                // the bootstrap: directional moves need an already-focused node, while one-
                // dimensional traversal assigns initial focus when there is none.
                val focusManager = androidx.compose.ui.platform.LocalFocusManager.current
                androidx.compose.runtime.DisposableEffect(Unit) {
                    sc2MoveFocus = { dir ->
                        focusManager.moveFocus(dir) ||
                            focusManager.moveFocus(androidx.compose.ui.focus.FocusDirection.Next)
                    }
                    onDispose { sc2MoveFocus = null }
                }
                Surface(modifier = Modifier.fillMaxSize()) { App(forceGamepadUi = forceGamepadUi) }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        // Keep `getIntent()` truthful for anything that reads it later (the gamepad-UI dev flag).
        setIntent(intent)
        deepLinkFrom(intent)?.let { pendingDeepLink = it }
    }

    /**
     * The `punktfunk://` URL of a VIEW intent, or null. Only VIEW: the launcher's MAIN intent
     * carries no data, and nothing else may inject a URL into the router.
     */
    private fun deepLinkFrom(intent: Intent?): String? =
        intent?.takeIf { it.action == Intent.ACTION_VIEW }?.data?.toString()

    override fun onResume() {
        super.onResume()
        startSc2MenuNav()
        maybeAskDsPermission()
    }

    override fun onPause() {
        // Release the claim while backgrounded so the OS (and other apps) get the pad back.
        stopSc2MenuNav()
        super.onPause()
    }

    override fun onDestroy() {
        sc2Receiver?.let { runCatching { unregisterReceiver(it) } }
        sc2Receiver = null
        stopSc2MenuNav()
        super.onDestroy()
    }

    /**
     * Engage the menu-time SC2 capture if possible: setting on, not streaming, and a wired/Puck
     * pad attached (asking for USB permission at most once per attach — [forceAsk] re-arms the
     * dialog, for the Controllers screen's explicit grant button) — else an already-paired BLE
     * controller, asking for Bluetooth access once if one appears to be attached
     * ([maybeAskSc2BtPermission]). Safe to call repeatedly.
     *
     * [sc2UsbAttached] is refreshed first, before any of the gates below: the console lists the
     * pad whether or not this client may capture it.
     */
    fun startSc2MenuNav(forceAsk: Boolean = false) {
        if (forceAsk) sc2PermissionAsked = false
        val usbManager = getSystemService(Context.USB_SERVICE) as UsbManager
        sc2UsbAttached = usbManager.deviceList.values.any {
            it.vendorId == Sc2Device.VID_VALVE && it.productId in Sc2Device.USB_PIDS
        }
        if (streamHandle != 0L) return // StreamScreen owns the pad while streaming
        if (sc2Menu?.isActive == true) return
        if (!SettingsStore(this).load().sc2Capture) return
        val cap = sc2Menu ?: io.unom.punktfunk.kit.Sc2Capture(this).also { c ->
            c.onUiKey = { key, down -> runOnUiThread { sc2NavKey(key, down) } }
            c.onActiveChanged = { on -> runOnUiThread { sc2MenuActive = on } }
            sc2Menu = c
        }
        val dev = cap.findUsbDevice()
        when {
            dev != null && usbManager.hasPermission(dev) -> cap.startUsb(dev)
            dev != null && !sc2PermissionAsked -> {
                sc2PermissionAsked = true
                usbManager.requestPermission(
                    dev,
                    PendingIntent.getBroadcast(
                        this, 1,
                        Intent(SC2_MENU_PERMISSION).setPackage(packageName),
                        // MUTABLE: the USB stack appends the grant extras to this intent.
                        PendingIntent.FLAG_MUTABLE,
                    ),
                )
            }
            dev == null && Sc2BleLink.permissionGranted(this) -> {
                cap.pairedBleAddress()?.let { cap.startBle(it) }
            }
            dev == null -> maybeAskSc2BtPermission()
        }
    }

    /**
     * Ask for Bluetooth access when a BLE-paired SC2 looks like it is attached and we cannot see
     * it — once per process, and never on a device that shows no sign of owning one.
     *
     * The permission is the whole reason a Bluetooth SC2 used to go unnoticed: the bonded list and
     * `connectGatt` both need it from API 31, nothing in the client had ever requested it, and the
     * bonded-list call answers an empty list rather than an error when it is missing — so the
     * capture stood down silently and the console UI never flipped to its controller layout, while
     * the same pad over USB worked (field report, 2026-08-15). Asking is gated on
     * [Gamepad.sc2InputDevicePresent] because an uncaptured SC2 sits in lizard mode as a
     * keyboard/mouse [android.view.InputDevice] — visible without any permission at all — so the
     * prompt reaches the people who have the hardware and nobody else.
     */
    private fun maybeAskSc2BtPermission() {
        val permission = Sc2BleLink.CONNECT_PERMISSION ?: return // granted at install time here
        if (sc2BtPermissionAsked) return
        if (!Gamepad.sc2InputDevicePresent()) return
        sc2BtPermissionAsked = true
        requestPermissions(arrayOf(permission), REQ_SC2_BLUETOOTH)
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<String>,
        grantResults: IntArray,
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        // Engage immediately on the grant — the pad is already paired, so there is nothing else to
        // wait for and the user just told us what they want it for.
        if (requestCode == REQ_SC2_BLUETOOTH &&
            grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED
        ) {
            startSc2MenuNav()
        }
    }

    /**
     * Buzz the captured SC2's own grip motors — the rumble test for a pad the input stack cannot
     * see, so [testRumble] has no `InputDevice` to reach for. False when nothing is captured.
     */
    fun testSc2Rumble(): Boolean = sc2Menu?.testRumble() == true

    /** Release the menu-time SC2 capture (backgrounded / stream taking over). Idempotent. */
    fun stopSc2MenuNav() {
        sc2Menu?.stop()
        sc2MenuActive = false
    }

    /**
     * Ask for USB access to an attached Sony pad the moment it appears — a fresh attach while
     * the app is open, or the app coming to the foreground with one already plugged in — at most
     * once per attach, so the stream-mode capture ([io.unom.punktfunk.kit.DsCapture]) engages
     * silently instead of interrupting stream start with the dialog. Unlike the SC2's menu flow
     * there is nothing to START on the grant: an uncaptured Sony pad is an ordinary InputDevice
     * at menu time, so the grant is simply recorded (Android keeps it while the pad stays
     * attached). The broadcast only refreshes the Controllers screen's card if it happens to be
     * open; a deny leaves that card's explicit button as the re-ask.
     */
    private fun maybeAskDsPermission() {
        if (streamHandle != 0L) return // StreamScreen owns its own permission flow while streaming
        if (dsPermissionAsked) return
        if (!SettingsStore(this).load().dsCapture) return
        val usbManager = getSystemService(Context.USB_SERVICE) as UsbManager
        val dev = usbManager.deviceList.values.firstOrNull {
            it.vendorId == DsDevice.VID_SONY && it.productId in DsDevice.USB_PIDS
        } ?: return
        if (usbManager.hasPermission(dev)) return
        dsPermissionAsked = true
        usbManager.requestPermission(
            dev,
            PendingIntent.getBroadcast(
                this, 4, // requestCode 4 — 0..3 are the SC2 stream/menu + DS stream/card grants
                Intent(DS_USB_PERMISSION_ACTION).setPackage(packageName),
                // MUTABLE: the USB stack appends the grant extras to this intent.
                PendingIntent.FLAG_MUTABLE,
            ),
        )
    }

    /**
     * One SC2 navigation key transition from the menu-time capture (main thread) — routed the
     * same way [dispatchKeyEvent]'s not-streaming branch routes a real pad's buttons: B backs,
     * A activates the focused element, everything else (D-pad, shoulders, Start/Select) goes to
     * the framework's focus navigation. Also claims the console-UI glyphs for the pad.
     */
    private fun sc2NavKey(keyCode: Int, down: Boolean) {
        if (streamHandle != 0L) return // raced a stream start — the wire path owns input now
        lastPadIsGamepad = true
        lastPadStyle = Gamepad.PadStyle.XBOX // Valve pads carry A/B/X/Y in Xbox positions
        val action = if (down) KeyEvent.ACTION_DOWN else KeyEvent.ACTION_UP
        // The console UI navigates through padKeyProbe, not the focus system, so a synthesized
        // key has to reach it exactly like a real one. The Skia probe asks the event's source,
        // and a 2-arg KeyEvent is the virtual keyboard — a captured pad read as a TV remote.
        padKeyProbe?.let { if (it(padKeyEvent(action, keyCode))) return }
        when (keyCode) {
            // B → back, on release (same edge the real-pad path uses).
            KeyEvent.KEYCODE_BUTTON_B -> if (!down) onBackPressedDispatcher.onBackPressed()
            // A → activate the focused element (the focus system understands DPAD_CENTER; the
            // Compose node focused via the moveFocus hook receives it once the ComposeView
            // holds view-focus).
            KeyEvent.KEYCODE_BUTTON_A ->
                super.dispatchKeyEvent(KeyEvent(action, KeyEvent.KEYCODE_DPAD_CENTER))
            // D-pad → Compose's own focus API (a synthetic DPAD KeyEvent can't grant initial
            // focus — see [sc2MoveFocus]); one move per press edge.
            KeyEvent.KEYCODE_DPAD_UP -> if (down) moveSc2Focus(androidx.compose.ui.focus.FocusDirection.Up)
            KeyEvent.KEYCODE_DPAD_DOWN -> if (down) moveSc2Focus(androidx.compose.ui.focus.FocusDirection.Down)
            KeyEvent.KEYCODE_DPAD_LEFT -> if (down) moveSc2Focus(androidx.compose.ui.focus.FocusDirection.Left)
            KeyEvent.KEYCODE_DPAD_RIGHT -> if (down) moveSc2Focus(androidx.compose.ui.focus.FocusDirection.Right)
            else -> super.dispatchKeyEvent(KeyEvent(action, keyCode))
        }
    }

    /** A capture-synthesized pad key, stamped SOURCE_GAMEPAD the way a real pad's event is. */
    private fun padKeyEvent(action: Int, keyCode: Int): KeyEvent {
        val now = SystemClock.uptimeMillis()
        return KeyEvent(
            now, now, action, keyCode, 0, 0,
            KeyCharacterMap.VIRTUAL_KEYBOARD, 0, 0, InputDevice.SOURCE_GAMEPAD,
        )
    }

    private fun moveSc2Focus(dir: androidx.compose.ui.focus.FocusDirection) {
        val hook = sc2MoveFocus
        if (hook == null || !hook(dir)) {
            // No composition hook (shouldn't happen) — fall back to the raw key dispatch.
            super.dispatchKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, dirToKey(dir)))
            super.dispatchKeyEvent(KeyEvent(KeyEvent.ACTION_UP, dirToKey(dir)))
        }
    }

    private fun dirToKey(dir: androidx.compose.ui.focus.FocusDirection): Int = when (dir) {
        androidx.compose.ui.focus.FocusDirection.Up -> KeyEvent.KEYCODE_DPAD_UP
        androidx.compose.ui.focus.FocusDirection.Down -> KeyEvent.KEYCODE_DPAD_DOWN
        androidx.compose.ui.focus.FocusDirection.Left -> KeyEvent.KEYCODE_DPAD_LEFT
        else -> KeyEvent.KEYCODE_DPAD_RIGHT
    }

    /**
     * Resolve the panel's highest-refresh mode at the user's resolution once, for
     * [setConsoleHighRefreshRate].
     *
     * Same resolution only: a pin to another size is a non-seamless mode switch that rescales every
     * app on open and again on exit, and `display.mode` is what [nativeDisplayMode] reads at connect.
     *
     * NEVER on a TV, which leaves the id at `0` and makes every [setConsoleHighRefreshRate] call a
     * no-op. A TV has no 60 Hz governor to defeat, and a menu-time pin makes "Native" negotiate the
     * pinned rate while [StreamScreen] releases the pin for the decoder's own HDMI switch — the panel
     * drops back under a host already serving the higher rate.
     */
    private fun resolveHighRefreshMode() {
        if (isTvDevice(this)) return
        highRefreshModeId = sameResolutionModes().maxByOrNull { it.refreshRate }?.modeId ?: 0
    }

    /** The display's modes at its current resolution, which is the one the user picked: no pin
     * this activity sets ever leaves it. */
    private fun sameResolutionModes(): List<android.view.Display.Mode> {
        @Suppress("DEPRECATION")
        val disp = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) display else windowManager.defaultDisplay
        val current = disp?.mode ?: return emptyList()
        return disp.supportedModes.filter {
            it.physicalWidth == current.physicalWidth && it.physicalHeight == current.physicalHeight
        }
    }

    /**
     * Opt the CONSOLE UI into the panel's highest refresh mode. Some OEMs (Nothing OS among them) pin
     * third-party apps to 60Hz unless they explicitly ask for more, which halves the smoothness of the
     * UI's scrolling/animation on a 120/144Hz panel. [StreamScreen] replaces this with
     * [setStreamDisplayMode] while streaming (matched to the video, not to the panel maximum).
     */
    fun setConsoleHighRefreshRate(high: Boolean) {
        if (highRefreshModeId == 0) return
        window.attributes = window.attributes.apply {
            preferredDisplayModeId = if (high) highRefreshModeId else 0
        }
    }

    /**
     * Pin the panel to a display mode matching the STREAM's refresh for the session's duration —
     * exact rate first, else the smallest integer multiple (120 for a 60 stream: judder-free 2:1
     * pulldown), else the highest available. Same-resolution modes only.
     *
     * The window-level mode pin is the belt to the decoder's `ANativeWindow_setFrameRate` braces:
     * the surface hint alone is advisory, and several OEM refresh governors (Nothing OS's LTPO
     * logic among them) ignore it entirely for third-party apps — leaving a 120 Hz session
     * presenting on a 60/90 Hz panel, which reads as judder + a refresh of extra latency. The
     * preferredDisplayModeId is the one signal they all honor. [hz] ≤ 0 falls back to releasing
     * the pin (the pre-pin behaviour).
     */
    fun setStreamDisplayMode(hz: Int) {
        if (hz <= 0) {
            setConsoleHighRefreshRate(false)
            return
        }
        val target = streamModeFor(hz) ?: return
        window.attributes = window.attributes.apply { preferredDisplayModeId = target.modeId }
    }

    /**
     * The panel refresh rate a [hz] stream runs against — [streamModeFor]'s pick, from the mode
     * TABLE rather than `display.refreshRate`. The distinction matters: under a per-uid frame
     * rate override (games get a 60 fps default on Android 15+) `refreshRate` reports the
     * override, not the panel — observed on-glass as a 120 Hz panel reading back as 60. The
     * supported-modes list is not override-filtered. `0` when unresolvable.
     */
    fun streamPanelFps(hz: Int): Int =
        streamModeFor(hz)?.refreshRate?.let { kotlin.math.round(it).toInt() } ?: 0

    /** The same-resolution display mode [setStreamDisplayMode] pins for a [hz] stream. */
    private fun streamModeFor(hz: Int): android.view.Display.Mode? {
        if (hz <= 0) return null
        val sameRes = sameResolutionModes()
        fun multiple(rate: Float): Int {
            val k = (rate / hz).toInt()
            return if (k >= 2 && kotlin.math.abs(rate - hz * k) < 1f) k else 0
        }
        return sameRes.minWithOrNull(
            compareBy(
                {
                    when {
                        kotlin.math.abs(it.refreshRate - hz) < 1f -> 0 // exact
                        multiple(it.refreshRate) > 0 -> 1 // integer multiple — prefer smallest
                        else -> 2 // no relation — prefer highest so at least nothing is halved
                    }
                },
                { if (multiple(it.refreshRate) > 0) it.refreshRate else -it.refreshRate },
            ),
        )
    }

    /** The grave key went down as Tab ([Keymap.altTabAlias]); its repeats and release follow. */
    private var graveAsTab = false

    /**
     * A key while streaming: true = ours (forwarded, claimed by the mouse/pad/ring, or swallowed),
     * false = the system's. Asked twice on a false answer — by the capture view before the IME
     * sees a hardware key, then by [dispatchKeyEvent] — so no false path has a side effect.
     */
    fun streamKey(event: KeyEvent): Boolean {
        val handle = streamHandle
        if (handle != 0L) {
            // A mouse's side buttons, when they arrive key-shaped, are X1/X2 — not the ring.
            // Resolved before the gamepad and remote-pointer hooks so neither can claim them as
            // its own BACK; [mouseSideButton] answers null for a pad's and a TV remote's.
            mouseSideButton(event)?.let { back ->
                when (event.action) {
                    KeyEvent.ACTION_DOWN ->
                        if (event.repeatCount == 0) mouseForwarder?.sideButtonKey(back, true)
                    KeyEvent.ACTION_UP -> mouseForwarder?.sideButtonKey(back, false)
                }
                return true
            }
            // Gamepad buttons (incl. DPAD only when truly from a gamepad — else KEYCODE_DPAD_* are
            // keyboard arrows and belong to the VK path below — and BACK, which is how a pad with
            // no BUTTON_SELECT scancode delivers its Select: see [Gamepad.padButtonBit], which is
            // why this asks it rather than `buttonBit`).
            if (fromPad(event)) {
                val key = Gamepad.padKeyCode(event)
                val bit = Gamepad.padButtonBit(key, event.flags)
                // A pad with no trigger axis sends its triggers as L2/R2 keys.
                if (bit == 0 && gamepadRouter?.onTriggerKey(event, key) == true) return true
                if (bit != 0) {
                    // The router forwards the bit on this device's own wire pad index and tracks held
                    // state per pad. The emergency-exit chord (Select + Start + L1 + R1) is handled
                    // inside the router: holding it briefly (~1 s, with an on-screen hint) fires
                    // router.onExitChord (wired in StreamScreen), so a couch user with no keyboard/Back
                    // can still leave — but an accidental brush of the four buttons no longer quits.
                    gamepadRouter?.onButton(event, bit)
                    return true // consumed
                }
            }
            // The ring is modal: its scrim owns every finger, the router masks the pad behind it,
            // and this is the same claim for keys. Non-nav keys are swallowed, not sent — except
            // modifiers, so the Ctrl+Alt+Shift that opened it still lift on the host.
            ringKeys?.let { nav ->
                if (!fromPad(event) && event.keyCode !in SYSTEM_KEYS &&
                    !KeyEvent.isModifierKey(event.keyCode)
                ) {
                    if (event.action == KeyEvent.ACTION_DOWN && event.repeatCount == 0) {
                        ringNavForKey(event.keyCode)?.let(nav)
                    }
                    return true
                }
            }
            // TV remote-as-pointer sees non-gamepad keys first (SELECT long-press toggles it;
            // while active it owns the D-pad/SELECT/PLAY-PAUSE/BACK).
            if (!event.isFromSource(InputDevice.SOURCE_GAMEPAD)) {
                remotePointer?.let { if (it.onKey(event)) return true }
            }
            // Ctrl+Alt+Shift+Q — the cross-client pointer-capture toggle chord. Swallow both
            // edges of the Q (the modifiers already went over the wire, exactly like desktop).
            if (event.keyCode == KeyEvent.KEYCODE_Q &&
                event.isCtrlPressed && event.isAltPressed && event.isShiftPressed
            ) {
                if (event.action == KeyEvent.ACTION_DOWN && event.repeatCount == 0) {
                    mouseForwarder?.toggleCapture()
                }
                return true
            }
            // Ctrl+Alt+Shift+O — the cross-client quick-action chord. It only opens: while the
            // ring is up it owns the keys above, and Escape closes it.
            if (event.keyCode == KeyEvent.KEYCODE_O &&
                event.isCtrlPressed && event.isAltPressed && event.isShiftPressed
            ) {
                if (event.action == KeyEvent.ACTION_DOWN && event.repeatCount == 0) openRing?.invoke()
                return true
            }
            when (event.keyCode) {
                // What [mouseSideButton] and the pad branch left: a FALLBACK duplicate of an
                // unconsumed BUTTON_* press, or a mouse-sourced BACK whose motion edge already went
                // (a captured mouse stamps SOURCE_MOUSE_RELATIVE). The rest open the ring.
                KeyEvent.KEYCODE_BACK, KeyEvent.KEYCODE_FORWARD ->
                    if (event.isFromSource(InputDevice.SOURCE_MOUSE) ||
                        event.isFromSource(InputDevice.SOURCE_MOUSE_RELATIVE) ||
                        event.flags and KeyEvent.FLAG_FALLBACK != 0
                    ) {
                        return true
                    }
                // Leave these to the system even while streaming.
                in SYSTEM_KEYS -> {}
                else -> {
                    val down = when (event.action) {
                        KeyEvent.ACTION_DOWN -> true
                        KeyEvent.ACTION_UP -> false
                        else -> return false
                    }
                    // Without the KEYBOARD grant the key path is inert: consumed (so nothing
                    // drives Android navigation under the stream) but never sent — the host
                    // would drop it, and the Access chip is what says why. Courtesy gating.
                    if (streamAccess and SessionAccess.KEYBOARD == 0) return true
                    // Full-event overload: evdev scancode first (positional under ANY selected
                    // physical-keyboard layout), keycode fallback — see Keymap docs.
                    var vk = Keymap.toVk(event)
                    // Android keeps Alt+Tab for its own switcher unless the key service is on;
                    // Alt+` is the stand-in.
                    if (vk == 0xC0 && !KeyCaptureService.running) {
                        val altOnly = event.isAltPressed && !event.isCtrlPressed && !event.isMetaPressed
                        graveAsTab = Keymap.altTabAlias(down, event.repeatCount > 0, altOnly, graveAsTab)
                        if (graveAsTab) vk = 0x09
                    }
                    if (vk != 0) {
                        // Soft-keyboard events (the IME's virtual device — the stream's
                        // KeyCaptureView path) carry Shift only as META state, where a real
                        // keyboard sends discrete Shift transitions — so mirror the meta bit as
                        // a VK_LSHIFT wrap or every IME capital/symbol lands unshifted on the
                        // host. Never applied to hardware events: their Shift already went over
                        // the wire, and a synthetic release here would un-hold a physical Shift
                        // the user is still pressing.
                        val imeShift = event.deviceId == KeyCharacterMap.VIRTUAL_KEYBOARD &&
                            event.isShiftPressed && vk != 0xA0 && vk != 0xA1
                        if (down && imeShift) NativeBridge.nativeSendKey(handle, 0xA0, true, 0)
                        NativeBridge.nativeSendKey(handle, vk, down, 0)
                        if (!down && imeShift) NativeBridge.nativeSendKey(handle, 0xA0, false, 0)
                        // The service path never sees a dispatcher repeat, so a held hardware
                        // key repeats from here. Modifiers do not repeat on Android either; a
                        // new key takes the repeat over, and its own release ends it.
                        val hardware = event.deviceId != KeyCharacterMap.VIRTUAL_KEYBOARD && !fromPad(event)
                        if (KeyCaptureService.running && hardware && !KeyEvent.isModifierKey(event.keyCode)) {
                            if (down && event.repeatCount == 0) armKeyRepeat(handle, vk)
                            else if (!down && vk == repeatVk) cancelKeyRepeat()
                        }
                        return true // consumed — don't let the system also act on it
                    }
                }
            }
        }
        return false
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (streamHandle != 0L) {
            if (streamKey(event)) return true
        } else {
            // Note which input the console UI is being driven by, so its glyphs match (a TV remote's
            // D-pad is not from SOURCE_GAMEPAD; a pad's face buttons / D-pad are) — and, for a real
            // pad, WHICH pad family, so the glyphs wear its lettering/shapes.
            if (event.action == KeyEvent.ACTION_DOWN && isConsoleNavKey(event.keyCode)) {
                lastPadIsGamepad = fromPad(event)
                if (lastPadIsGamepad) {
                    lastPadStyle = Gamepad.styleFor(event.device)
                    lastPadDeviceId = event.deviceId
                }
            }
            // The Controllers debug screen sees pad events before the navigation remap below.
            padKeyProbe?.let { if (it(event)) return true }
            if (fromPad(event)) {
                // Not streaming: a game controller drives the Compose UI (TV + phone). Map the face
                // buttons to the navigation the focus system / back stack understand; D-pad *keys*
                // already move focus on their own, so they fall through to super untouched. Read
                // through [Gamepad.padKeyCode] so a pad Android has no key layout for reaches the
                // menus on the right buttons too, not only the stream.
                when (Gamepad.padKeyCode(event)) {
                    // B → back. Drive the OnBackPressedDispatcher directly rather than synthesising a
                    // BACK KeyEvent: a synthetic event isn't "tracking", so the framework's default
                    // onKeyUp(BACK) never calls onBackPressed() and Compose BackHandlers wouldn't fire.
                    KeyEvent.KEYCODE_BUTTON_B -> {
                        if (event.action == KeyEvent.ACTION_UP) onBackPressedDispatcher.onBackPressed()
                        return true
                    }
                    // A → activate the focused element (the focus system understands DPAD_CENTER).
                    KeyEvent.KEYCODE_BUTTON_A ->
                        return super.dispatchKeyEvent(KeyEvent(event.action, KeyEvent.KEYCODE_DPAD_CENTER))
                }
            }
        }
        return super.dispatchKeyEvent(event)
    }

    /**
     * Did this key event come from a controller — the question every pad branch here actually
     * means when it asks `isFromSource(SOURCE_GAMEPAD)`. The rule, and why it is drawn where it
     * is, lives on [Gamepad.eventFromPad].
     *
     * Only the menus take the Steam Controller 2's identity fallback. While streaming, an
     * uncaptured SC2 is a keyboard and a mouse, and its keys have to keep typing at the host.
     */
    private fun fromPad(event: KeyEvent): Boolean =
        Gamepad.eventFromPad(event, includeSc2Fallback = streamHandle == 0L)

    /**
     * `true` (back) / `false` (forward) when this key event is a MOUSE side button, null when it
     * belongs to the ring or a pad. The rule, and why a TV differs, is [isMouseSideKey].
     *
     * A mouse that carries its side buttons on the HID consumer page (AC Back / AC Forward) reaches
     * us only as `KEYCODE_BACK`/`KEYCODE_FORWARD`, with no `BUTTON_BACK`/`BUTTON_FORWARD` motion
     * edge behind it, and may stamp them SOURCE_KEYBOARD — so the DEVICE is what we ask. A captured
     * mouse reports SOURCE_MOUSE_RELATIVE instead of SOURCE_MOUSE; both count.
     */
    private fun mouseSideButton(event: KeyEvent): Boolean? {
        val back = when (event.keyCode) {
            KeyEvent.KEYCODE_BACK -> true
            KeyEvent.KEYCODE_FORWARD -> false
            else -> return null
        }
        val device = event.device ?: return null
        val claimed = isMouseSideKey(
            tv = isTv,
            external = device.isExternalDevice(),
            pad = fromPad(event) || Gamepad.isPad(device),
            fallback = event.flags and KeyEvent.FLAG_FALLBACK != 0,
            mouse = device.supportsSource(InputDevice.SOURCE_MOUSE) ||
                device.supportsSource(InputDevice.SOURCE_MOUSE_RELATIVE),
            dpad = device.supportsSource(InputDevice.SOURCE_DPAD),
            mousePresent = hasPhysicalMouse(),
        )
        return if (claimed) back else null
    }

    /** Last D-pad direction synthesised from a stick/HAT — edge detection (one focus move per push). */
    private var lastNavDir = 0

    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        if (streamHandle != 0L) {
            if (gamepadRouter?.onMotion(event) == true) return true
            // Physical mouse (uncaptured): hover motion, wheel, button edges.
            if (event.isFromSource(InputDevice.SOURCE_MOUSE)) {
                mouseForwarder?.let { if (it.onGenericMotion(event)) return true }
            }
            return super.dispatchGenericMotionEvent(event)
        }
        // The Controllers debug screen sees pad motion before the stick→D-pad synthesis below.
        padMotionProbe?.let { if (it(event)) return true }
        // Not streaming: turn the gamepad HAT / left stick into discrete D-pad focus moves, so a
        // controller navigates the menus even when its D-pad reports as axes (not key events) and
        // for stick-based navigation. Edge-detected so a held direction moves focus exactly once.
        if (event.isFromSource(InputDevice.SOURCE_JOYSTICK) ||
            event.isFromSource(InputDevice.SOURCE_GAMEPAD)
        ) {
            val x = event.getAxisValue(MotionEvent.AXIS_HAT_X)
                .let { if (it != 0f) it else event.getAxisValue(MotionEvent.AXIS_X) }
            val y = event.getAxisValue(MotionEvent.AXIS_HAT_Y)
                .let { if (it != 0f) it else event.getAxisValue(MotionEvent.AXIS_Y) }
            val dir = when {
                x <= -0.5f -> KeyEvent.KEYCODE_DPAD_LEFT
                x >= 0.5f -> KeyEvent.KEYCODE_DPAD_RIGHT
                y <= -0.5f -> KeyEvent.KEYCODE_DPAD_UP
                y >= 0.5f -> KeyEvent.KEYCODE_DPAD_DOWN
                else -> 0
            }
            if (dir != lastNavDir) {
                lastNavDir = dir
                if (dir != 0) {
                    lastPadIsGamepad = true // a stick/HAT push can only come from a real gamepad
                    lastPadStyle = Gamepad.styleFor(event.device)
                    lastPadDeviceId = event.deviceId
                    super.dispatchKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, dir))
                    super.dispatchKeyEvent(KeyEvent(KeyEvent.ACTION_UP, dir))
                    return true
                }
            } else if (dir != 0) {
                return true // already moved for this push; swallow until the stick returns to centre
            }
        }
        return super.dispatchGenericMotionEvent(event)
    }

    /**
     * Mouse clicks/drags ride the TOUCH stream (the pointer is "down"). While streaming they
     * belong to the mouse forwarder, never to the Compose touch-gesture layer — a physical
     * mouse click must be a real click at the cursor, not a synthesized trackpad tap.
     */
    override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
        if (streamHandle != 0L && ev.isFromSource(InputDevice.SOURCE_MOUSE)) {
            mouseForwarder?.let { if (it.onTouchEvent(ev)) return true }
        }
        return super.dispatchTouchEvent(ev)
    }

    /** The OS is the source of truth for pointer capture (it releases on focus loss). */
    override fun onPointerCaptureChanged(hasCapture: Boolean) {
        super.onPointerCaptureChanged(hasCapture)
        mouseForwarder?.onCaptureChanged(hasCapture)
    }

    /** Keys that drive the console UI — D-pad + face buttons; used to classify the last input source. */
    private fun isConsoleNavKey(kc: Int): Boolean = when (kc) {
        KeyEvent.KEYCODE_DPAD_UP, KeyEvent.KEYCODE_DPAD_DOWN, KeyEvent.KEYCODE_DPAD_LEFT,
        KeyEvent.KEYCODE_DPAD_RIGHT, KeyEvent.KEYCODE_DPAD_CENTER, KeyEvent.KEYCODE_ENTER,
        -> true
        else -> KeyEvent.isGamepadButton(kc)
    }

    /** Does [intent]'s link resolve to the host [live] is already streaming? */
    private fun targetsHost(intent: Intent?, live: SessionGate.Live): Boolean {
        val url = deepLinkFrom(intent) ?: return false
        val parsed = DeepLinks.parse(url) as? DeepLinkResult.Parsed ?: return false
        val target = DeepLinks.resolveHost(parsed.link, KnownHostStore(this).all())
        return target is HostResolution.Record && target.host.id == live.hostId
    }
}
