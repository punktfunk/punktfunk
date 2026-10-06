package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.usb.UsbDevice
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.nio.ByteBuffer

/**
 * One captured Steam Controller 2 — the glue between a transport link ([Sc2UsbLink] /
 * [Sc2BleLink]) and one of two consumers:
 *
 * **Stream mode** (`router != null`, owned by StreamScreen):
 * - **Raw plane (the point):** every input report goes to core
 *   ([GamepadRouter.ExternalPad.hidReport]) for the host's as-is virtual `28DE:1302` pad, which
 *   Steam Input drives like the physical controller. Core gates it as on every client: the frozen
 *   IMU block, the ring, the system-button policy.
 * - **Typed mirror:** buttons/sticks/triggers are ALSO diffed onto the ordinary per-transition
 *   plane, so the emergency exit chord works, and a host that degraded the kind (no UHID → the
 *   Xbox 360 pad) still gets a playable controller.
 * - **Raw return:** the host's hidraw writes (Steam's `0x80` rumble output reports, lizard/IMU
 *   feature settings) arrive via [GamepadFeedback.onHidRaw] → [onHidRaw] → the link, landing on
 *   the real controller's motors/firmware.
 *
 * **UI mode** (`router == null`, owned by MainActivity while NOT streaming): the lizard-mode
 * kb/mouse never produces gamepad events, so an uncaptured SC2 can't drive the console UI at
 * all. Here the parsed state is edge-detected into [onUiKey] navigation transitions instead
 * (D-pad + face buttons + Start/Select; the left stick synthesizes one D-pad step per push,
 * mirroring MainActivity's stick-to-focus behavior for ordinary pads).
 *
 * The wire slot is claimed lazily on the FIRST state report — a Puck with no controller powered
 * on stays invisible to the host — and released (with a wireless-disconnect event or on [stop])
 * so pad indices never leak. A dropped BLE link releases the slot but keeps its transport
 * SELECTED, because [Sc2BleLink] re-acquires by itself; the slot comes back with the reports.
 *
 * Report callbacks arrive on the link's own thread; the router's slot table and chord timer are
 * thread-safe for this (same contract as the feedback poll threads), and UI-mode consumers hop to
 * the main thread themselves.
 */
class Sc2Capture(
    context: Context,
    private val router: GamepadRouter? = null,
) {
    private val usb = Sc2UsbLink(context, ::onReport, ::onLinkClosed)
    private val ble = Sc2BleLink(context, ::onReport, ::onLinkClosed)
    @Volatile private var activeLink: Int = LINK_NONE

    /** True when the USB link is a Puck dongle — the only transport whose wireless-status
     *  reports are authoritative. A WIRED pad also emits them, truthfully reporting "no radio
     *  link" — acting on that tore the slot down 255 ms after creation (first on-glass run). */
    @Volatile private var dongleLink = false

    @Volatile private var pad: GamepadRouter.ExternalPad? = null

    /** Set while [claim] reads the pad's identity; reports until then are dropped. */
    @Volatile private var claiming = false

    /** Bumped by [releaseSlot]: a claim that finishes after it opens no slot. */
    @Volatile private var epoch = 0
    private val rawBuf: ByteBuffer = ByteBuffer.allocateDirect(64)

    // Typed-mirror diff state (wire units).
    private val state = Sc2Device.State()
    private var mirror = TypedMirror()

    /** Report ids seen so far — each logged once, for remote diagnosis of what the pad emits. */
    private val seenIds = HashSet<Int>()

    /** Whether the live transport is delivering reports. Deliberately not `activeLink !=
     *  LINK_NONE`: a BLE link stays selected while it re-acquires a pad that was switched off. */
    @Volatile private var reporting = false

    // UI-mode state (router == null): held navigation keys + the stick's current synth direction.
    private var uiHeld = HashSet<Int>()
    private var uiStickDir = 0

    /**
     * UI-mode sink: one navigation key transition (an Android `KeyEvent.KEYCODE_*`), invoked on
     * the LINK thread — the consumer hops to the main thread. Set before [startUsb]/[startBle].
     */
    @Volatile
    var onUiKey: ((keyCode: Int, down: Boolean) -> Unit)? = null

    /**
     * Fired (link thread) when the capture engages or drops — lets the app surface "SC2
     * connected" in the console-UI gate and the Controllers screen.
     */
    @Volatile
    var onActiveChanged: ((active: Boolean) -> Unit)? = null

    val isActive: Boolean get() = activeLink != LINK_NONE

    /** First attached SC2/Puck USB device, for the permission flow. */
    fun findUsbDevice(): UsbDevice? = usb.findDevice()

    /**
     * The first already-bonded BLE Steam Controller's address, or null. The caller checks
     * BLUETOOTH_CONNECT first (without it the bonded list reads as empty anyway).
     */
    fun pairedBleAddress(): String? = ble.pairedControllers().firstOrNull()?.address

    /** Start capturing [dev] over USB (permission already granted). */
    fun startUsb(dev: UsbDevice): Boolean {
        if (activeLink != LINK_NONE) return false
        val ok = usb.start(dev)
        if (ok) {
            activeLink = LINK_USB
            dongleLink = dev.productId != Sc2Device.PID_WIRED
            reporting = true
            onActiveChanged?.invoke(true)
        }
        return ok
    }

    /** Start capturing the bonded BLE controller at [address]. */
    fun startBle(address: String): Boolean {
        if (activeLink != LINK_NONE) return false
        val ok = ble.start(address)
        if (ok) {
            activeLink = LINK_BLE
            reporting = true
            onActiveChanged?.invoke(true)
        }
        return ok
    }

    /** Replay a host raw write on the physical pad — wire to [GamepadFeedback.onHidRaw]. */
    fun onHidRaw(padIndex: Int, kind: Int, data: ByteArray) {
        if (padIndex != pad?.index) return // addressed to some other controller
        writeLink(kind, data)
    }

    /**
     * Buzz both grip motors for [RUMBLE_MS] — the client's own rumble test, since a captured pad
     * leaves the input stack and has no Android vibrator to pulse. False when no link is
     * delivering reports. The stop frame is not optional: `0x80` carries a level the firmware
     * holds until the next one.
     */
    fun testRumble(): Boolean {
        if (!reporting) return false
        writeLink(HID_RAW_OUTPUT, Sc2Device.rumbleFrame(0xFFFF, 0xFFFF))
        Handler(Looper.getMainLooper()).postDelayed(
            { writeLink(HID_RAW_OUTPUT, Sc2Device.rumbleFrame(0, 0)) },
            RUMBLE_MS,
        )
        return true
    }

    /** Hand one id-first report to whichever transport is selected; no link, no write. */
    private fun writeLink(kind: Int, data: ByteArray) {
        when (activeLink) {
            LINK_USB -> usb.writeRaw(kind, data)
            LINK_BLE -> ble.writeRaw(kind, data)
        }
    }

    /** Stop the link and free the wire slot (host tears the virtual pad down). Idempotent. */
    fun stop() {
        val wasActive = activeLink != LINK_NONE
        // The firmware holds the last rumble level it got: put the motors down before letting go.
        // USB goes EP0-direct, since the interrupt queue stops draining with the reader.
        when (activeLink) {
            LINK_USB -> usb.writeControl(RUMBLE_STOP)
            LINK_BLE -> ble.writeRaw(HID_RAW_OUTPUT, RUMBLE_STOP)
        }
        when (activeLink) {
            LINK_USB -> usb.stop()
            LINK_BLE -> ble.stop()
        }
        activeLink = LINK_NONE
        dongleLink = false
        reporting = false
        releaseSlot()
        releaseUiKeys()
        if (wasActive) onActiveChanged?.invoke(false)
    }

    // ---- link callbacks (link thread) ----

    private fun onReport(report: ByteArray, len: Int) {
        if (len == 0) return // a zero-length BLE notification has no id to read
        if (!reporting) {
            reporting = true
            onActiveChanged?.invoke(true) // a re-acquired BLE pad is live again
        }
        val id = report[0].toInt() and 0xFF
        if (seenIds.add(id)) Log.i(TAG, "SC2 report id=0x%02x seen (len=%d)".format(id, len))
        // Wireless status: authoritative ONLY through a Puck dongle (powering the pad off frees
        // its wire index + the host's virtual device). A wired/BLE pad emits it too — truthfully
        // saying "no radio link" — and must NOT tear the slot down (SDL's wired path likewise
        // marks the controller connected unconditionally and reconnects on any state report).
        if ((id == Sc2Device.ID_WIRELESS || id == Sc2Device.ID_WIRELESS_X) && len >= 2) {
            if (dongleLink) {
                if (report[1].toInt() and 0xFF == Sc2Device.WIRELESS_DISCONNECT) {
                    Log.i(TAG, "Puck reports controller powered off — releasing wire slot")
                    releaseSlot()
                    releaseUiKeys()
                }
            }
            return
        }
        if (!Sc2Device.parseState(report, len, state)) {
            // Battery/status and future report types still belong to the as-is stream.
            forwardRaw(report, len)
            return
        }
        if (router == null) {
            mirrorUi()
            return
        }
        val p = pad ?: return claim(router)
        forwardRaw(report, len)
        mirrorTyped(p)
    }

    /**
     * Read the pad's identity, then open its wire slot and send the identity: the host builds the
     * virtual pad from it. On its own thread — a query through a Puck blocks up to a second, and a
     * GATT read must not wait inside a GATT callback. A full slot table drops reports until one frees.
     */
    private fun claim(router: GamepadRouter) {
        if (claiming) return
        claiming = true
        val (puck, link, started) = Triple(dongleLink, activeLink, epoch)
        Thread({
            val identity = runCatching { readIdentity(link) }.getOrNull()
            synchronized(this) {
                if (started == epoch) {
                    val pref = if (puck) Gamepad.PREF_STEAMCONTROLLER2_PUCK else Gamepad.PREF_STEAMCONTROLLER2
                    pad = router.openExternal(pref)?.also { p ->
                        identity?.let { (serial, replies) -> p.identity(serial, replies) }
                        val via = if (puck) "Puck" else "direct"
                        Log.i(TAG, "SC2 captured → wire pad ${p.index} ($via passthrough, serial ${identity?.first})")
                    }
                }
                claiming = false
            }
        }, "pf-sc2-claim").start()
    }

    /**
     * The serial and packed replies the host mirrors: the USB serial over a cable or Puck, the
     * engraved serial (`0xAE` attribute 1) over Bluetooth. Null when the pad answered nothing.
     */
    private fun readIdentity(link: Int): Pair<String, ByteArray>? {
        val packed = java.io.ByteArrayOutputStream()
        var unitSerial = ""
        var answered = 0
        var asked = 0
        while (true) {
            val request = NativeBridge.nativeSc2IdentityRequest(dongleLink, asked) ?: break
            asked++
            val reply = when (link) {
                LINK_USB -> usb.exchange(request)
                LINK_BLE -> ble.exchange(request)
                else -> null
            } ?: continue
            for (part in arrayOf(request, reply)) {
                val n = part.size.coerceAtMost(64)
                packed.write(n)
                packed.write(part, 0, n)
            }
            answered++
            if (request.contentEquals(SERIAL_QUERY)) {
                unitSerial = reply.drop(4).takeWhile { it != 0.toByte() }.toByteArray().decodeToString()
            }
        }
        val serial = (if (link == LINK_USB) usb.serialNumber() else null) ?: unitSerial
        Log.i(TAG, "SC2 identity $serial, $answered/$asked replies")
        return if (answered == 0 && serial.isEmpty()) null else serial to packed.toByteArray()
    }

    private fun forwardRaw(report: ByteArray, len: Int) {
        val p = pad ?: return
        val n = len.coerceAtMost(rawBuf.capacity())
        rawBuf.clear()
        rawBuf.put(report, 0, n)
        p.hidReport(rawBuf, n)
    }

    /** Diff the parsed state onto the per-transition plane (buttons + axes, on change only). */
    private fun mirrorTyped(p: GamepadRouter.ExternalPad) = mirror.push(
        p, Sc2Device.wireButtons(state.buttons),
        state.lsX, state.lsY, state.rsX, state.rsY, state.lt, state.rt,
    )

    /**
     * UI mode: edge-detect the parsed state into navigation key transitions. Buttons map to
     * their Android keycodes (press AND release, so the focus system sees real holds); the left
     * stick synthesizes ONE D-pad step per push past half deflection — the same single-move
     * behavior MainActivity gives ordinary pads' sticks.
     */
    private fun mirrorUi() {
        val sink = onUiKey ?: return
        val held = HashSet<Int>(8)
        var i = 0
        while (i < UI_KEY_MAP.size) {
            if (state.buttons and UI_KEY_MAP[i] != 0) held.add(UI_KEY_MAP[i + 1])
            i += 2
        }
        for (key in held) if (key !in uiHeld) sink(key, true)
        for (key in uiHeld) if (key !in held) sink(key, false)
        uiHeld = held
        // Left stick → a HELD D-pad direction (device convention: +y = up): pressed while
        // deflected, released on centre/direction change. The console UI's probe machinery
        // turns a held direction into its own auto-repeat, exactly like a physical D-pad; the
        // focus-hook path moves once per press edge either way.
        val dir = when {
            state.lsX <= -STICK_NAV -> android.view.KeyEvent.KEYCODE_DPAD_LEFT
            state.lsX >= STICK_NAV -> android.view.KeyEvent.KEYCODE_DPAD_RIGHT
            state.lsY >= STICK_NAV -> android.view.KeyEvent.KEYCODE_DPAD_UP
            state.lsY <= -STICK_NAV -> android.view.KeyEvent.KEYCODE_DPAD_DOWN
            else -> 0
        }
        if (dir != uiStickDir) {
            // The D-pad bits share these keycodes; don't release a direction the physical
            // D-pad itself still holds (uiHeld tracks the button-sourced state).
            if (uiStickDir != 0 && uiStickDir !in uiHeld) sink(uiStickDir, false)
            if (dir != 0 && dir !in uiHeld) sink(dir, true)
            uiStickDir = dir
        }
    }

    /** Release every held UI-mode key (link drop / stop) so nothing sticks in the focus system. */
    private fun releaseUiKeys() {
        val sink = onUiKey
        if (sink != null) {
            for (key in uiHeld) sink(key, false)
            if (uiStickDir != 0 && uiStickDir !in uiHeld) sink(uiStickDir, false)
        }
        uiHeld = HashSet()
        uiStickDir = 0
    }

    private fun onLinkClosed() {
        Log.i(TAG, "SC2 link closed (unplug / power-off)")
        releaseSlot()
        releaseUiKeys()
        reporting = false
        // BLE holds a standing connection request, so leave the transport selected: host raw
        // writes keep routing to it and the next state report re-opens the slot. USB has no such
        // request — release it (see the note in DsCapture.onLinkClosed) and go idle, or the same
        // process leaks a link per power-cycle, which the Puck does many times in one session.
        if (activeLink == LINK_USB) {
            activeLink = LINK_NONE
            dongleLink = false
            usb.stop()
        }
        onActiveChanged?.invoke(false)
    }

    private fun releaseSlot() = synchronized(this) {
        epoch++
        pad?.close()
        pad = null
        mirror = TypedMirror()
    }

    private companion object {
        const val TAG = "Sc2Capture"
        const val LINK_NONE = 0
        const val LINK_USB = 1
        const val LINK_BLE = 2

        val RUMBLE_STOP = Sc2Device.rumbleFrame(0, 0)

        /** The `0xAE` query for attribute 1, the engraved serial. */
        val SERIAL_QUERY = byteArrayOf(0x01, 0xAE.toByte(), 0x15, 0x01)

        /** The C ABI's `HID_RAW_OUTPUT` — the kind every output report goes out as. */
        const val HID_RAW_OUTPUT = 0

        /** Matches the vibrator pulse the ordinary pad test gives: long enough to feel, short
         *  enough that a stuck stop frame is not a running motor. */
        const val RUMBLE_MS = 300L

        /** Half deflection (device i16 range) — the stick-to-focus threshold. */
        const val STICK_NAV = 16384

        /** UI-mode mapping: SC2 button bit → Android keycode, as (bit, key) pairs. */
        val UI_KEY_MAP = intArrayOf(
            Sc2Device.DPAD_UP, android.view.KeyEvent.KEYCODE_DPAD_UP,
            Sc2Device.DPAD_DOWN, android.view.KeyEvent.KEYCODE_DPAD_DOWN,
            Sc2Device.DPAD_LEFT, android.view.KeyEvent.KEYCODE_DPAD_LEFT,
            Sc2Device.DPAD_RIGHT, android.view.KeyEvent.KEYCODE_DPAD_RIGHT,
            Sc2Device.A, android.view.KeyEvent.KEYCODE_BUTTON_A,
            Sc2Device.B, android.view.KeyEvent.KEYCODE_BUTTON_B,
            Sc2Device.X, android.view.KeyEvent.KEYCODE_BUTTON_X,
            Sc2Device.Y, android.view.KeyEvent.KEYCODE_BUTTON_Y,
            Sc2Device.LB, android.view.KeyEvent.KEYCODE_BUTTON_L1,
            Sc2Device.RB, android.view.KeyEvent.KEYCODE_BUTTON_R1,
            Sc2Device.MENU, android.view.KeyEvent.KEYCODE_BUTTON_START,
            Sc2Device.VIEW, android.view.KeyEvent.KEYCODE_BUTTON_SELECT,
        )
    }
}
