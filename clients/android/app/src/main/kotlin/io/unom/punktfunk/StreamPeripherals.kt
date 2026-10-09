package io.unom.punktfunk

import android.content.BroadcastReceiver
import android.content.Context
import android.hardware.usb.UsbManager
import android.util.Log
import android.view.SurfaceView
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.unit.IntSize
import io.unom.punktfunk.kit.DeviceGyro
import io.unom.punktfunk.kit.DsCapture
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.GamepadFeedback
import io.unom.punktfunk.kit.GamepadRouter
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.PadSensors
import io.unom.punktfunk.kit.Sc2Capture
import io.unom.punktfunk.kit.SessionAccess
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.kit.deviceBodyVibrator

/**
 * Everything that runs beside the picture for one session — the pad router and its chords, the
 * physical mouse and the TV remote-as-pointer, the shared clipboard, host→pad feedback, the phone
 * and pad sensors, and the two USB captures — built in [start] and released in [stop], in the
 * order the handle's lifetime needs. Writes its state through [ui]; reads the views it needs
 * through the lambdas, which resolve live (the views attach after this object exists).
 */
internal class StreamPeripherals(
    private val context: Context,
    private val activity: MainActivity?,
    private val session: ActiveSession,
    private val ui: StreamUi,
    private val ring: RingState,
    private val haptics: ConsoleHaptics,
    private val isTv: Boolean,
    private val stopMic: () -> Unit,
    private val keyCapture: () -> KeyCaptureView?,
    private val videoView: () -> SurfaceView?,
    private val containerSize: () -> IntSize,
    private val video: () -> VideoFrame,
    private val onSessionEnded: (SessionEndReason) -> Unit,
) {
    private val handle = session.handle
    private val settings = session.settings
    private lateinit var router: GamepadRouter
    private lateinit var mouse: MouseForwarder
    private var remote: RemotePointer? = null
    private var clip: ClipboardSync? = null
    private lateinit var feedback: GamepadFeedback
    private var phoneGyro: DeviceGyro? = null
    private var padSensors: PadSensors? = null
    private var sc2: Sc2Capture? = null
    private var sc2UsbReceiver: BroadcastReceiver? = null
    private var ds: DsCapture? = null
    private var dsUsbReceiver: BroadcastReceiver? = null
    private var decor: android.view.View? = null
    private var priorPointerIcon: android.view.PointerIcon? = null
    private val hiddenPointer =
        android.view.PointerIcon.getSystemIcon(context, android.view.PointerIcon.TYPE_NULL)

    fun start() {
        activity?.streamHandle = handle // route hardware keys to this session
        // Multi-controller router: a stable wire pad index per connected controller, per-device axis
        // state, Arrival/Remove on hot-plug, and feedback routed back by pad index. Forwards every
        // controller (Automatic). Built here, released on dispose.
        router = GamepadRouter(
            context, handle, settings.gamepad, settings.gamepadForwarding,
            settings.systemButtonsForward(), settings.guideGestureEnabled(),
        )
        activity?.gamepadRouter = router
        // Every controller that was already connected got a slot in the router's constructor, so
        // this is the session's pad answer at t=0 — what the start banner's words are chosen from.
        ui.padPresent = router.forwardedDevices().isNotEmpty()
        // Select+Start+L1+R1 chord leaves the stream — a deliberate quit (signal it so the host skips
        // the keep-alive linger), unlike a host-ended / backgrounded drop. The router debounces it
        // (must be held ~1.5 s) and fires onExitChord on its main-thread timer, so leave the stream
        // the same way the Back gesture does.
        activity?.requestStreamExit = { NativeBridge.nativeDisconnectQuit(handle); onSessionEnded(SessionEndReason.LOCAL) }
        router.onExitChord = { activity?.requestStreamExit?.invoke() }
        // Show a "hold to quit" hint the moment the chord completes (the router debounces the actual
        // exit); it clears when the buttons release early or the hold elapses. Runs on the main thread.
        router.onExitArmed = { armed -> ui.exitArming = armed }
        // Select + Y toggles the mic — with no on-screen mute element, this chord is the whole of
        // the control. Ignored when no capture is running (there is nothing to mute, and a hint
        // saying "Microphone muted" over a mic nobody opened would be a lie).
        // A captured Sony pad whose motion this session cannot carry. Fires once per pad, at the
        // moment it is claimed, on the main thread.
        router.onMotionUnreachable = { ui.motionHint = true }
        router.onMicChord = {
            if (ui.micRunning) {
                val next = !ui.micMuted
                ui.mute(next)
                ui.micHint = if (next) "Microphone muted" else "Microphone live"
            }
        }
        // Select + X steps the stats overlay one tier — the same live cycle the three-finger tap
        // performs, and the ONLY route to it on a TV or in a passthrough-touch session. Session-
        // local on purpose: this mirrors the tap exactly (`onCycleStats` below), and the settings
        // row calls it a live cycle — the stored default is what the next stream starts from.
        router.onStatsChord = { ui.statsVerbosity = ui.statsVerbosity.next() }
        // `Select+A` opens the ring at the screen centre; while it is up the pad belongs to it.
        val openRing = { ring.openAt(Offset(containerSize().width / 2f, containerSize().height / 2f)) }
        router.onRingChord = { pad ->
            haptics.confirm()
            openRing()
            ring.opener = pad
        }
        // Ctrl+Alt+Shift+O, the cross-client chord, opens the same ring from a keyboard.
        activity?.openRing = openRing
        router.onRingNav = { ring.nav(it) }
        // Both of the ring's claims on its input, taken and dropped on the same edge: the pad
        // through the router, everything key-shaped through the activity. A TV remote is neither a
        // finger nor a pad, so without the second one the ring opens and cannot be driven.
        ring.onOpenChange = { open ->
            router.setRingOpen(open)
            activity?.ringKeys = if (open) ({ nav -> ring.nav(nav) }) else null
            // A mouse drives the ring like a finger: its own pointer, and no click lands in the game.
            mouse.setSuspended(open)
            decor?.pointerIcon = if (open) priorPointerIcon else hiddenPointer
        }
        // Physical mouse: uncaptured hover/click/wheel forwards as absolute pointing; captured
        // (setting or the Ctrl+Alt+Shift+Q chord) raw deltas forward as relative mouse-look.
        // The local cursor is hidden over the stream — the host's own cursor, composited into
        // the video, is the one the user sees (twin of the desktop clients' hidden cursor).
        decor = activity?.window?.decorView
        priorPointerIcon = decor?.pointerIcon
        decor?.pointerIcon = hiddenPointer
        val viewConfig = android.view.ViewConfiguration.get(context)
        mouse = MouseForwarder(
            handle,
            captureWanted = settings.mouseMode == MouseMode.CAPTURE,
            // A touchpad's ACTION_SCROLL axes price distance through the OS scroll factors;
            // the display density turns those pixels into the wire's DIP.
            scrollFactorV = viewConfig.scaledVerticalScrollFactor,
            scrollFactorH = viewConfig.scaledHorizontalScrollFactor,
            density = context.resources.displayMetrics.density,
            // Window point → frame pixel through the picture's placement (see MouseForwarder.frameAt)
            // — read live, so it is right from the frame the SurfaceView is first laid out. The
            // SurfaceView sits at the placement's rect, so the container's origin is its origin
            // less that offset.
            frameAt = { wx, wy ->
                videoView()?.takeIf { it.width > 0 && it.height > 0 }?.let { v ->
                    val map = video().at(containerSize())
                    if (map.isEmpty) return@let null
                    val loc = IntArray(2)
                    v.getLocationInWindow(loc)
                    val cx = wx - (loc[0] - map.placement.dstX)
                    val cy = wy - (loc[1] - map.placement.dstY)
                    intArrayOf(map.x(cx), map.y(cy), map.width, map.height)
                }
            },
        )
        mouse.onRequestCapture = {
            // The grab needs the (focusable) capture view: focus it, then ask. Posted so a
            // request racing view attach/focus settles on the next frame.
            keyCapture()?.let { v ->
                v.post {
                    v.requestFocus()
                    v.requestPointerCapture()
                }
            }
        }
        // Capture is the window's, so the decor view releases it too — and unlike the capture
        // view it is still attached when stop() runs from the composable's dispose.
        mouse.onReleaseCapture = { decor?.releasePointerCapture() }
        activity?.mouseForwarder = mouse
        // TV remote-as-pointer: hold SELECT ≈ 0.8 s to toggle; the D-pad then glides the host
        // cursor (see RemotePointer). TV only — a phone's remote-less keys stay on the VK path.
        remote = if (isTv) {
            RemotePointer(
                handle,
                // Moves are host pixels, so the glide scales with the stream's width, not the TV's.
                surfaceWidth = {
                    video().width.takeIf { it > 0 }
                        ?: videoView()?.width?.takeIf { it > 0 } ?: decor?.width ?: 1920
                },
                onActiveChanged = { on -> ui.remotePointerOn = on },
                // The toggle TYPES — summoning also needs the KEYBOARD grant (hiding is free).
                onKeyboardToggle = {
                    keyCapture()?.let { v ->
                        if (v.imeShown || ui.accessGrants and SessionAccess.KEYBOARD != 0) {
                            v.setImeVisible(!v.imeShown)
                        }
                    }
                },
            )
        } else {
            null
        }
        activity?.remotePointer = remote
        // Everything the grant gates hang off now exists — apply the session's access level once
        // up front (the poll only re-applies on change, and a restricted session is restricted
        // from its first event, not from its first poll).
        applyAccess(ui.accessGrants)
        // Shared clipboard (text v1): only when the user setting is on AND the session's access
        // includes the clipboard AND the host has a working clipboard service. Ungranted, the
        // host's policy resolution declines everything anyway (grants AND into it); not starting
        // the sync is the client-side mirror — no offers announced, no poll thread for a plane
        // that cannot move. Applied at session start only, like the host's own coordinator gate.
        clip = if (session.clipboardSync &&
            ui.accessGrants and SessionAccess.CLIPBOARD != 0 &&
            NativeBridge.nativeClipSupported(handle)
        ) {
            ClipboardSync(context, handle).also { it.start() }
        } else {
            null
        }
        // Host→client feedback (rumble + DualSense lightbar/LEDs), routed to each controller by pad
        // index via the router; poll threads stopped + joined before the router is released and the
        // session closed. The device's own vibrator plays a motorless built-in pad's rumble, and
        // with "Rumble on this phone" (opt-in) it also mirrors controller 1's.
        feedback = GamepadFeedback(
            handle,
            router,
            bodyVibrator = deviceBodyVibrator(context),
            mirrorPad0 = settings.rumbleOnPhone,
            rumble = settings.padRumble,
        ).also { it.start() }
        // "Gyro from this phone" (opt-in): this device's IMU speaks for controller 1's motion
        // while wire pad 0 is a controller without a gyro of its own — the rumble mirror's
        // sibling, data flowing the other way. The mirror gates itself per sample (it stands
        // down whenever pad 0's controller has motion of its own — a capture link below, or a
        // pad whose own sensors PadSensors is reading), so it composes without coordination here.
        phoneGyro = if (settings.gyroOnPhone && settings.gamepadForwarding) {
            DeviceGyro(context, handle, router).also { it.start() }
        } else {
            null
        }
        // A Bluetooth controller's OWN gyro, through the platform sensor framework (API 31+):
        // a BT DualSense / DS4 / Switch Pro / 8BitDo is an ordinary InputDevice, so none of the
        // capture links below ever sees it and its motion used to go nowhere at all. No separate
        // setting — this is the pad's own IMU doing what the pad is for, and unlike the USB
        // captures it claims nothing; forwarding being off is the only thing that silences it.
        padSensors = if (settings.gamepadForwarding) {
            PadSensors(router).also { it.start() }
        } else {
            null
        }
        // Free a disconnected controller's rumble/lights bindings promptly (else the open lights
        // session leaks until the session ends), and take its sensor listeners off with it — the
        // same callback also fires when a USB capture below CLAIMS the pad, which is what keeps
        // the claimed pad from being fed motion twice. The router owns hot-plug; the feedback owns
        // the binds. Assigned before the captures are constructed, so their claims land on it.
        router.onSlotClosed = { deviceId ->
            feedback.onDeviceRemoved(deviceId)
            padSensors?.onSlotClosed(deviceId)
        }
        // The other edge: a controller that arrives (or first speaks) mid-session gets its sensors
        // read too. The pads already connected were swept by PadSensors.start() above — both run
        // on the main thread with nothing between them, so no controller falls through the gap.
        router.onSlotOpened = { deviceId ->
            padSensors?.onSlotOpened(deviceId)
            // A pad that wakes up a second into the stream still deserves the chord banner — the
            // desktop rebuilds its banner text every frame for exactly this case.
            ui.padPresent = true
        }
        // Steam Controller 2 as-is passthrough (opt-out): capture a wired/Puck USB pad — or an
        // already-paired BLE one — and forward its raw reports; the host mirrors a real
        // 28DE:1302 that its Steam drives directly, and Steam's rumble/settings writes come back
        // through feedback.onHidRaw onto the physical controller. Engages only when such a pad is
        // actually present; the wire slot is claimed lazily on its first state report.
        // The menu-time capture (UI navigation) must let go before the stream-mode capture can
        // claim the interfaces; it resumes in onDispose once the stream releases them.
        activity?.stopSc2MenuNav()
        val sc2 = if (settings.gamepadForwarding) {
            Sc2Capture(context, router)
        } else {
            null
        }
        this.sc2 = sc2
        if (sc2 != null) {
            feedback.onHidRaw = sc2::onHidRaw
            val usbDev = sc2.findUsbDevice()
            if (usbDev != null) {
                sc2UsbReceiver = requestUsbCapture(
                    context, usbDev, "io.unom.punktfunk.SC2_USB_PERMISSION", 0, "SC2", sc2::startUsb,
                )
            } else {
                // No USB pad: fall back to a bonded BLE one. The Bluetooth-permission gate lives
                // inside pairedBleAddress() (it answers null, and says why, when the grant is
                // missing) rather than being restated here — the grant itself is asked for where
                // a user can act on it, in the console UI and the Controllers screen.
                sc2.pairedBleAddress()?.let { addr ->
                    Log.i("punktfunk", "SC2: no USB pad — using the paired BLE controller $addr")
                    sc2.startBle(addr)
                }
            }
        }
        // Sony pad capture (DualSense / Edge / DualShock 4, opt-out): claim a USB-connected
        // pad's HID interface and drive it directly — rumble without a kernel force-feedback
        // driver, plus adaptive triggers, lightbar, player LEDs and gyro/touchpad, none of which
        // the InputDevice path can render (no platform API for any of them). Uncaptured (toggle
        // off / permission denied / Bluetooth) the pad stays on the ordinary InputDevice path —
        // the automatic fallback. Host feedback routes back through feedback.sink; the claim
        // frees the pad's InputDevice slot itself (see DsCapture.startUsb), so the wire index
        // hands over deterministically.
        val ds = if (settings.dsCapture && settings.gamepadForwarding) {
            DsCapture(context, router)
        } else {
            null
        }
        this.ds = ds
        if (ds != null) {
            feedback.sink = ds
            // Tier-A pad audio: render the host's 0xD1 streams on the pad's own 4-channel USB
            // audio device. Bound here rather than inside DsCapture because the session handle
            // lives at this layer; DsCapture decides WHEN (it knows the wire index and the link
            // lifetime), this decides WHETHER.
            if (settings.padHaptics || settings.padSpeaker) {
                ds.padAudio = object : DsCapture.PadAudioHook {
                    override fun start(pad: Int, fd: Int) {
                        val ok = NativeBridge.nativeStartPadAudio(
                            handle,
                            pad,
                            fd,
                            settings.padHaptics,
                            settings.padSpeaker,
                        )
                        Log.i("punktfunk", "pad audio on pad $pad: ${if (ok) "started" else "unavailable"}")
                    }

                    // Returns only once the render thread is joined — DsCapture calls this before
                    // closing the connection whose descriptor that thread borrows.
                    override fun stop(pad: Int) = NativeBridge.nativeStopPadAudio(handle, pad)
                }
            }
            // Its OWN action, not the Controllers screen's [DS_USB_PERMISSION_ACTION] — that one is
            // registered by MainActivity and the console shell too, and a stream must not answer
            // their grants. requestCode 2: 0/1 are the SC2 stream/menu grants.
            ds.findUsbDevice()?.let { usbDev ->
                dsUsbReceiver = requestUsbCapture(
                    context, usbDev, "io.unom.punktfunk.DS_USB_PERMISSION", 2, "Sony pad", ds::startUsb,
                )
            }
        }
        activity?.recaptureUsbPads = ::recaptureUsbPads
    }

    /**
     * A pad plugged back in mid-stream: capture it again when its grant already stands. An unplug
     * ends the link; without this the pad stays on the plain input path for the rest of the
     * session. Never asks — a permission dialog does not belong in the middle of a game.
     */
    private fun recaptureUsbPads() {
        val usb = context.getSystemService(Context.USB_SERVICE) as UsbManager
        ds?.takeIf { !it.isActive }?.findUsbDevice()?.takeIf(usb::hasPermission)?.let { ds?.startUsb(it) }
        sc2?.takeIf { !it.isActive }?.findUsbDevice()?.takeIf(usb::hasPermission)?.let { sc2?.startUsb(it) }
    }

    /**
     * Push a grant mask into every gate that consults one — at session start (once the
     * router/forwarders exist) and again whenever the poll sees the mask change (an AccessUpdate
     * revoked or restored something mid-session). The gates it does NOT reach — the Compose-side
     * ones (the touch layer, the IME summon, the banner line, the chip) — key on
     * `ui.accessGrants` directly and re-run on the state write.
     */
    fun applyAccess(grants: Int) {
        activity?.streamAccess = grants
        activity?.gamepadRouter?.gamepadGranted = grants and SessionAccess.GAMEPAD != 0
        val pointerOk = grants and SessionAccess.POINTER != 0
        activity?.mouseForwarder?.let { m ->
            m.pointerGranted = pointerOk
            // A revocation must also let an existing grab go (and lift held buttons): a captured
            // mouse that moves nothing reads as a broken mouse, not a spectator session.
            if (!pointerOk) m.release()
        }
        activity?.remotePointer?.setGranted(pointerOk)
        // Mic revoked mid-session: stop the capture — the host detaches its end regardless, and
        // an open mic (with the platform's recording indicator lit) feeding a plane the host
        // drops would be the worst kind of lie. Not restarted on a re-grant: the host attaches
        // the mic service at session setup only, so a fresh session is the honest offer.
        if (grants and SessionAccess.MIC == 0 && ui.micRunning) stopMic()
    }

    /** Release in the order the handle's lifetime needs; the caller closes the handle after. */
    fun stop() {
        activity?.recaptureUsbPads = null
        clip?.stop() // stop + join the clipboard poll thread BEFORE the handle is freed
        feedback.onHidRaw = null
        feedback.sink = null
        feedback.stop() // stop + join the poll threads BEFORE the router is released / handle freed
        phoneGyro?.stop() // join the sensor thread + park pad 0's rotation at zero, same ordering rule
        // After the mirror, so it cannot resume writing pad 0 in the gap when a pad's own
        // sensors let go of it; before the router is released, so the parks still find slots.
        padSensors?.stop()
        sc2UsbReceiver?.let { runCatching { context.unregisterReceiver(it) } }
        sc2?.stop() // release the USB/BLE link + free the wire slot (host tears the pad down)
        dsUsbReceiver?.let { runCatching { context.unregisterReceiver(it) } }
        ds?.stop() // rumble-stop on the physical pad + release the USB link + free the wire slot
        router.onExitArmed = null // don't poke Compose state from release()'s disarm while tearing down
        router.onMicChord = null // same: no mute toggle on buttons released during teardown
        router.onStatsChord = null // same: no tier cycle on buttons released during teardown
        router.onRingChord = null
        router.onRingNav = null
        ring.onOpenChange = null
        activity?.ringKeys = null // a session torn down with the ring up must not keep the keys
        activity?.openRing = null
        router.onMotionUnreachable = null // same: no notice raised by a slot closing at teardown
        router.release() // flush every slot (nothing sticks host-side) + drop the hot-plug listener
        activity?.gamepadRouter = null
        // Mouse/remote-pointer teardown: lift held buttons, drop the grab, restore the cursor.
        mouse.release()
        activity?.mouseForwarder = null
        remote?.release()
        activity?.remotePointer = null
        decor?.pointerIcon = priorPointerIcon
        activity?.streamHandle = 0L
        activity?.streamAccess = SessionAccess.ALL // grants are per session, like the handle
        activity?.requestStreamExit = null
        // Back in the menus: the SC2 (if present) resumes driving the console UI.
        activity?.startSc2MenuNav()
    }
}
