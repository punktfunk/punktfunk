package io.unom.punktfunk.kit

import kotlin.math.abs
import kotlin.math.atan2
import kotlin.math.hypot
import android.content.Context
import android.hardware.input.InputManager
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import java.util.Collections
import java.util.concurrent.ConcurrentHashMap

/**
 * Multi-controller router for one stream session — the Android analogue of the Linux client's gamepad
 * `Worker`/`Slot` model (`pf-client-core/src/gamepad.rs`) over the shared native-plane wire contract
 * (`punktfunk-core/src/input.rs`). Each physical controller (Android `deviceId`) gets a STABLE
 * lowest-free wire pad index (0..15) held for its lifetime and freed only on disconnect, so a pad
 * dropping never renumbers the others (a game must not see its players shuffle). Every forwarded event
 * carries that pad index; a [NativeBridge.nativeSendGamepadArrival] declaring the pad's type is sent
 * once BEFORE its first input, a [NativeBridge.nativeSendGamepadRemove] on disconnect. Per-device axis
 * state lives in each slot's [Gamepad.AxisMapper] so a second controller can't clobber the first.
 * Feedback (rumble / HID) is routed BACK to the originating device by pad index via [deviceForPad].
 *
 * Selection: forward EVERY real controller (the Linux client's single-player pin has no Android UI
 * surface yet — Automatic is the only mode). A device gets its slot on its first pad input
 * ([opensSlotOnKey], or stick motion), never for being connected: TV remotes and system devices
 * declare gamepad buttons they never press. Released on detach.
 *
 * A single controller lands on wire index 0, so its per-transition button/axis wire is byte-identical
 * to the old single-pad path (plus the Arrival/Remove declarations the contract requires — which an
 * older host simply ignores).
 *
 * Threading: slot mutation + dispatch run on the main thread (Android input dispatch and the
 * InputManager hot-plug callbacks both land there). [deviceForPad] is read from the feedback poll
 * threads, [padPresent]/[padHasOwnMotion] from the phone-gyro thread and [deviceMotion] from the
 * pad-sensor thread, so the slot table is a [ConcurrentHashMap].
 */
/** What a pad does to the quick-action ring while it owns the pad (design §2.6): the D-pad steps
 *  the highlight, the left stick aims it, A fires, B backs out, Y returns it to the centre. */
sealed interface RingNav {
    data object Up : RingNav
    data object Down : RingNav
    data object Left : RingNav
    data object Right : RingNav
    data object Confirm : RingNav
    data object Back : RingNav
    data object Centre : RingNav

    /** The left stick's 60° sector: slot [slot] clockwise from 12 o'clock, null back at neutral.
     *  A dial follows the thumb — stepping one disc per push is the thing it must not do. */
    data class Sector(val slot: Int?) : RingNav
}

/**
 * The ring's other input vocabulary: a TV remote's or keyboard's keys, mapped the way the router
 * maps a pad's buttons. A remote is not a gamepad — no `SOURCE_GAMEPAD`, no `BUTTON_*` — so it
 * reaches the router through nothing, and the ring it just opened would be undrivable without
 * this. Null means the ring has no meaning for the key; the caller swallows it either way, because
 * a key aimed at a menu must not reach the game under it.
 */
fun ringNavForKey(keyCode: Int): RingNav? = when (keyCode) {
    KeyEvent.KEYCODE_DPAD_UP -> RingNav.Up
    KeyEvent.KEYCODE_DPAD_DOWN -> RingNav.Down
    KeyEvent.KEYCODE_DPAD_LEFT -> RingNav.Left
    KeyEvent.KEYCODE_DPAD_RIGHT -> RingNav.Right
    KeyEvent.KEYCODE_DPAD_CENTER, KeyEvent.KEYCODE_ENTER,
    KeyEvent.KEYCODE_NUMPAD_ENTER, KeyEvent.KEYCODE_SPACE,
    -> RingNav.Confirm
    KeyEvent.KEYCODE_BACK, KeyEvent.KEYCODE_ESCAPE -> RingNav.Back
    else -> null
}

class GamepadRouter(
    context: Context,
    private val handle: Long,
    setting: Int,
    /**
     * Forward this device's controllers to the host at all (`Settings.gamepadForwarding`,
     * default true). Off is for a couch whose controller reaches the host another way — USB
     * passthrough such as VirtualHere, or a pad plugged into the host itself — where forwarding
     * as well would give the host two pads for one pair of hands.
     *
     * Off still opens slots and tracks held state; it only stops the wire sends. That is
     * deliberate: the shortcuts are read off the same slots, and a couch that lost its quit
     * shortcut because a forwarding preference was off would be the worse bug. Nothing is
     * claimed by keeping a slot — the Android input stack shares controllers — unlike the USB
     * capture links, which `StreamScreen` does not start at all while this is off.
     */
    forwarding: Boolean = true,
    /**
     * Forward raw guide/QAM presses (`Settings.systemButtons` resolved — auto = forward on
     * Android, where the press reaches the app on most devices; `local` exists for
     * cross-client preset parity with the Gaming-Mode clients). Off keeps them entirely
     * with this device.
     */
    private val systemForward: Boolean = true,
    /**
     * This session's pad shortcuts, the actions it cannot perform already left out. One
     * [Shortcuts.Engine] per slot reads them before the ring's mask: what it forwards reaches
     * the host or the ring, what it fires always acts. [Shortcuts.Action.Ring] counts only while
     * [onRingChord] listens.
     */
    private val shortcuts: List<Shortcuts.Binding> = emptyList(),
) {

    /** The ctor's forwarding preference, fixed for the session — one term of [forwarding]. */
    private val forwardingSetting = forwarding

    /** The controller type every controller declares (`Gamepad.PREF_*`, Auto = each as itself):
     *  the ctor's setting until [setPadType] changes it for this stream. */
    @Volatile
    var padType: Int = setting
        private set

    /**
     * Whether this session's access includes the GAMEPAD grant ([SessionAccess.GAMEPAD]) —
     * seeded from the Welcome and kept live by `StreamScreen`'s access poll (an `AccessUpdate`
     * can revoke or restore it mid-session, latest-wins). Gates exactly what the forwarding
     * preference gates: the wire sends, never the slots — the shortcuts must keep working on a
     * Controller-less access level too (they are local controls that happen to be read off pad
     * buttons). The host enforces regardless; this stops the client paying to send
     * events that will be dropped. Volatile: the sensor and USB-capture threads read it per
     * sample through [forwarding].
     */
    @Volatile
    var gamepadGranted: Boolean = true

    /** Send on the wire at all — the forwarding preference AND the session's GAMEPAD grant. */
    private val forwarding: Boolean get() = forwardingSetting && gamepadGranted

    /** One forwarded controller: its stable wire pad index, per-device axis state, and held buttons. */
    private class Slot(
        val index: Int,
        val mapper: Gamepad.AxisMapper,
        /**
         * Whether motion sent for this pad can reach the game at all, asked once at open off the
         * kind it declared ([NativeBridge.nativePadMotionReaches]). False means the host built it a
         * backend with no motion plane, so [deviceMotion] drops the sample here rather than paying
         * to send one the host will decode and discard — at a controller's full sensor rate, for
         * the whole session. The capture-link pads carry the same flag on [ExternalPad].
         */
        val motionReaches: Boolean = true,
        /**
         * Whether [Gamepad.BTN_MISC1] means a MUTE button on this particular pad — the one bit
         * whose physical meaning differs per controller, and the gate on the mic toggle in
         * [slotButton].
         *
         * A DualSense has one; a Steam Controller 2 puts its QAM button on the same wire bit
         * (`Sc2Device`), and QAM must not mute anyone's microphone. Asked once at open, off the
         * fact each path actually knows: the report order for an [InputDevice] (only
         * [Gamepad.PadButtons.GENERIC_SONY] mints this bit there), the declared pad kind for a
         * capture link.
         */
        val hasMuteButton: Boolean = false,
        /**
         * Whether this pad brings a motion source of its own, so the phone-gyro mirror stands
         * down for it ([padHasOwnMotion]): every capture link's pad. A real controller earns it
         * later through [PadSensors]; the on-screen pad never does.
         */
        val ownMotion: Boolean = false,
        /** The kind this slot's Arrival declared; [setPadType] re-plugs the slots it moves. */
        val declared: Int = Gamepad.PREF_AUTO,
        /** This pad's shortcuts: every transition goes through it before the wire. */
        val engine: Shortcuts.Engine,
    ) {
        /** Button bits down on the host (Gamepad.BTN_*): what [releaseHeld] lifts. */
        var held = 0
    }

    /** deviceId → slot. Concurrent: the feedback poll threads read it via [deviceForPad]. */
    private val slots = ConcurrentHashMap<Int, Slot>()

    /**
     * deviceIds whose own gyro [PadSensors] is currently reading — see [setDeviceHasSensorMotion].
     * Written on the main thread, read from the phone-gyro thread, hence a concurrent set.
     */
    private val sensorDevices: MutableSet<Int> =
        Collections.newSetFromMap(ConcurrentHashMap<Int, Boolean>())

    /**
     * Invoked (main thread) with the deviceId whenever a slot closes — hot-unplug, a capture link's
     * [releaseDevice] claim, or session teardown. `StreamScreen` wires this to
     * `GamepadFeedback.onDeviceRemoved` so a disconnected pad's rumble / lights bindings are
     * released promptly instead of leaking until the feedback threads stop, and to
     * [PadSensors.onSlotClosed] so the controller's own sensor listeners come off with it.
     */
    var onSlotClosed: ((deviceId: Int) -> Unit)? = null

    /**
     * Invoked (main thread) with the deviceId whenever a slot opens for a REAL controller — its
     * first pad input. Not fired for [openExternal]: a capture link's pad has no [InputDevice]
     * behind it and streams motion from its own IMU already. `StreamScreen` wires this to
     * [PadSensors.onSlotOpened].
     */
    var onSlotOpened: ((deviceId: Int) -> Unit)? = null

    /**
     * Invoked (main thread) when a pad fires End stream — by default the exit chord held 1 s —
     * after its held buttons are lifted on the host. `StreamScreen` wires this to the
     * deliberate-quit exit.
     */
    var onExitChord: (() -> Unit)? = null

    /**
     * Invoked (main thread) with `true` while a pad holds an End stream chord that has not fired
     * yet, `false` once it fires or a button lifts. `StreamScreen` shows "Hold to quit…" from it.
     */
    var onExitArmed: ((armed: Boolean) -> Unit)? = null

    /**
     * Invoked (main thread) when a pad's own mute button ([Gamepad.BTN_MISC1] — a DualSense's) is
     * pressed: the couch's microphone toggle. The press still reaches the host.
     */
    var onMicChord: (() -> Unit)? = null

    /**
     * A pad fired [Shortcuts.Action.Ring], by default `Select + A`: the quick-action ring's opener.
     * On a gamepad-only session this is the only route to the ring: Back is a wire button while
     * streaming, and the twist needs a touchscreen. Carries the pad's wire index.
     */
    var onRingChord: ((Int) -> Unit)? = null
        set(value) {
            field = value
            pushSc2Gate()
        }

    /** A pad press while the ring owns the pad ([setRingOpen]). Main thread. */
    var onRingNav: ((RingNav) -> Unit)? = null

    /**
     * A pad fired any other action: the ring's own effect for that slot, a key combination's keys
     * down and up by [Shortcuts.Phase], a host action armed. Main thread. Ring, End stream, Guide
     * and Quick access never come here: the router acts on those itself.
     */
    var onShortcut: ((Shortcuts.Action, Shortcuts.Phase) -> Unit)? = null

    @Volatile private var ringOpen = false
    /** The ring sector the left stick last resolved to — see [ringSector]. */
    private var stickSector: Int? = null

    /**
     * The ring is up: everything held is released on the host NOW (a held sprint must not
     * survive a menu), and until it closes every forwarded press becomes [onRingNav] instead of a
     * wire send; shortcuts still fire. On close nothing is replayed — the next physical press
     * re-establishes itself.
     */
    fun setRingOpen(open: Boolean) {
        if (ringOpen == open) return
        ringOpen = open
        stickSector = null
        if (open) slots.values.forEach { releaseHeld(it) }
        pushSc2Gate()
    }

    /**
     * Core holds back of a Steam Controller 2's raw reports what this router holds back of the
     * typed plane: everything while the ring is up, Steam and QAM under a local system-button
     * policy, and the ring's own Select+A. Harmless on a pad that sends no raw reports.
     */
    private fun pushSc2Gate() {
        val gate = (if (ringOpen) SC2_GATE_MASKED else 0) or
            (if (!systemForward) SC2_GATE_SYSTEM_LOCAL else 0) or
            (if (onRingChord != null) SC2_GATE_CHORDS else 0)
        slots.values.forEach { NativeBridge.nativeSetSc2Gate(handle, it.index, gate) }
    }

    /**
     * A one-shot synthetic tap of a system button on the host's pad (pf-client-core's
     * `GamepadService.tapButton`), on the first forwarded slot's wire index — pad 0 when none
     * is open, which is best-effort (the host pad may not exist). [Gamepad.BTN_MISC1] is the
     * Quick Access action, pressed as that pad's host device reads it
     * ([Gamepad.quickAccessPresses]). Deliberately past [slotButton]: this is the ring's own
     * press, not the player's, so neither [ringOpen] nor the system-button policy may swallow it.
     */
    fun tapButton(bit: Int) {
        val slot = slots.values.minByOrNull { it.index }
        sendPresses(slot?.index ?: 0, tapEvents(systemPresses(slot, bit)))
    }

    /** What a Guide or Quick access press puts on the wire, in order. [Gamepad.BTN_MISC1] is Quick
     *  access as [slot]'s host pad reads it ([Gamepad.quickAccessPresses]). */
    private fun systemPresses(slot: Slot?, bit: Int): List<Int> = if (bit == Gamepad.BTN_MISC1) {
        // -1: no forwarded pad declared a kind, so the session default answers.
        Gamepad.quickAccessPresses(NativeBridge.nativePadHasQuickAccess(handle, slot?.declared ?: -1))
    } else {
        listOf(bit)
    }

    /** [tapEvents]' timed transitions on wire pad [pad], past the ring's mask and the policy. */
    private fun sendPresses(pad: Int, events: List<Triple<Int, Boolean, Long>>) {
        if (!forwarding) return
        for ((b, down, afterMs) in events) {
            if (afterMs == 0L) {
                NativeBridge.nativeSendGamepadButton(handle, b, down, pad)
            } else {
                mainHandler.postDelayed({ NativeBridge.nativeSendGamepadButton(handle, b, down, pad) }, afterMs)
            }
        }
    }

    private fun ringNavFor(bit: Int): RingNav? = when (bit) {
        Gamepad.BTN_DPAD_UP -> RingNav.Up
        Gamepad.BTN_DPAD_DOWN -> RingNav.Down
        Gamepad.BTN_DPAD_LEFT -> RingNav.Left
        Gamepad.BTN_DPAD_RIGHT -> RingNav.Right
        Gamepad.BTN_A -> RingNav.Confirm
        Gamepad.BTN_B -> RingNav.Back
        Gamepad.BTN_Y -> RingNav.Centre
        else -> null
    }

    /**
     * Invoked (main thread) once per pad when a captured controller WITH a gyro turns out to be in
     * a session whose virtual pad has no motion plane — its motion is not being sent, because every
     * sample would be decoded and dropped host-side.
     *
     * It exists because the failure is otherwise completely silent: the gyro just does nothing, and
     * from the couch that is indistinguishable from a broken sensor. The fix is the Controller type
     * setting, so whatever shows this has to name it. `StreamScreen` wires it to a brief notice.
     */
    var onMotionUnreachable: (() -> Unit)? = null

    private val mainHandler = Handler(Looper.getMainLooper())

    /** Ticks every engine when the next hold or owed release comes due ([scheduleTick]). */
    private val tick = Runnable {
        val now = SystemClock.uptimeMillis()
        for (slot in slots.values.toList()) apply(slot, slot.engine.tick(now))
        settle()
    }

    /** What [onExitArmed] last said. */
    private var exitArmed = false

    private val inputManager = context.getSystemService(InputManager::class.java)
    private val listener = object : InputManager.InputDeviceListener {
        override fun onInputDeviceAdded(deviceId: Int) {}
        override fun onInputDeviceRemoved(deviceId: Int) = closeSlot(deviceId)
        override fun onInputDeviceChanged(deviceId: Int) {}
    }

    init {
        inputManager?.registerInputDeviceListener(listener, mainHandler)
    }

    /**
     * One gamepad button transition for the device that produced [event] (already resolved to BTN_*
     * bit [bit]). False when the device has no slot and this key may not open one
     * ([opensSlotOnKey]): the caller handles it as a plain key. Otherwise the slot's shortcuts
     * read it ([slotButton]) and what they forward goes out on the slot's pad index.
     */
    fun onButton(event: KeyEvent, bit: Int): Boolean {
        val dev = event.device ?: return false
        val slot = slots[dev.id]
            ?: if (opensSlotOnKey(KeyEvent.isGamepadButton(event.keyCode), Gamepad.hasStick(dev))) {
                slotFor(dev)
            } else {
                null
            }
            ?: return false
        when (event.action) {
            // repeatCount guard: don't re-send a held button as auto-repeat.
            KeyEvent.ACTION_DOWN -> slotButton(slot, bit, down = true, send = event.repeatCount == 0)
            KeyEvent.ACTION_UP -> slotButton(slot, bit, down = false, send = true)
        }
        return true
    }

    /**
     * An L2/R2 key edge. True when [event]'s pad has no trigger axis
     * ([Gamepad.PadMap.digitalTriggers]), so the key IS its trigger; false leaves the event alone.
     */
    fun onTriggerKey(event: KeyEvent, keyCode: Int): Boolean {
        val left = when (keyCode) {
            KeyEvent.KEYCODE_BUTTON_L2 -> true
            KeyEvent.KEYCODE_BUTTON_R2 -> false
            else -> return false
        }
        val dev = event.device ?: return false
        // Asked before [slotFor], so a false answer opens no slot.
        if (!Gamepad.padMap(dev).digitalTriggers) return false
        val slot = slotFor(dev) ?: return false
        if (!ringOpen && forwarding) slot.mapper.onTriggerKey(left, event.action == KeyEvent.ACTION_DOWN)
        return true
    }

    /**
     * Is this bit's WIRE SEND kept with this device, though the bit is otherwise tracked normally?
     *
     * Exactly one is: a real mute button ([Slot.hasMuteButton]) under the "local" [systemForward]
     * policy. It still toggles the mic and reaches the shortcuts, but every send site has to ask,
     * including [releaseHeld]'s close-time flush, or a mute held across a disconnect would put a
     * release on the wire for a press that never went out. Every other system button under that
     * policy leaves [slotButton] at the top and never reaches a send at all.
     */
    private fun localOnly(slot: Slot, bit: Int): Boolean =
        !systemForward && bit == Gamepad.BTN_MISC1 && slot.hasMuteButton

    /**
     * One button transition on [slot] — the shared body behind [onButton] and an [ExternalPad]'s
     * transitions. An auto-repeat ([send] false) is no transition. The slot's engine reads the
     * press before the ring's mask; [apply] routes what it says.
     */
    private fun slotButton(slot: Slot, bit: Int, down: Boolean, send: Boolean) {
        if (down && !send) return
        // Raw system buttons stay local under the "local" policy, on both edges, so they reach
        // no shortcut either. A Steam Controller 2's QAM button is BTN_MISC1 and keeps that.
        //
        // A real MUTE button ([Slot.hasMuteButton]) is exempt: that policy keeps the button with
        // this device, and toggling this device's microphone is what a mute button does. It
        // loses only its wire send ([localOnly]).
        if (!systemForward &&
            (bit == Gamepad.BTN_GUIDE || (bit == Gamepad.BTN_MISC1 && !slot.hasMuteButton))
        ) {
            return
        }
        // BTN_MISC1 is also a Steam Controller 2's QAM, which must not mute anyone.
        if (down && bit == Gamepad.BTN_MISC1 && slot.hasMuteButton) onMicChord?.invoke()
        apply(slot, slot.engine.step(bit, down, SystemClock.uptimeMillis()))
        settle()
    }

    /** One engine's outputs for [slot]: a forward obeys the ring's mask, a fire and a lift do not. */
    private fun apply(slot: Slot, outs: List<Shortcuts.Out>) {
        for (o in outs) {
            when (o) {
                is Shortcuts.Out.Forward ->
                    if (!ringOpen) {
                        wire(slot, o.id, o.down)
                    } else if (o.down) {
                        ringNavFor(o.id)?.let { onRingNav?.invoke(it) }
                    }
                is Shortcuts.Out.Lift -> o.ids.forEach { wire(slot, it, false) }
                is Shortcuts.Out.Fire -> fire(slot, o.action, o.phase)
            }
        }
    }

    /** One transition on [slot]'s wire pad. A release of a button the host does not hold (the
     *  ring's flush lifted it) sends nothing. */
    private fun wire(slot: Slot, bit: Int, down: Boolean) {
        if (!down && slot.held and bit == 0) return
        if (forwarding && !localOnly(slot, bit)) NativeBridge.nativeSendGamepadButton(handle, bit, down, slot.index)
        slot.held = if (down) slot.held or bit else slot.held and bit.inv()
    }

    /** A pad shortcut fired. Guide and Quick access go out on this pad's wire index. */
    private fun fire(slot: Slot, action: Shortcuts.Action, phase: Shortcuts.Phase) {
        val id = (action as? Shortcuts.Action.Slot)?.slot
        when {
            action == Shortcuts.Action.Ring -> onRingChord?.invoke(slot.index)
            id == SlotId.EndStream -> {
                releaseHeld(slot)
                onExitChord?.invoke()
            }
            id == SlotId.Guide || id == SlotId.Qam -> {
                val events = tapEvents(systemPresses(slot, if (id == SlotId.Guide) Gamepad.BTN_GUIDE else Gamepad.BTN_MISC1))
                sendPresses(
                    slot.index,
                    when (phase) {
                        Shortcuts.Phase.DOWN -> events.filter { it.second }
                        // A tap's releases trail its last press, so an early lift cannot pass it.
                        Shortcuts.Phase.UP -> events.filter { !it.second }
                        else -> events
                    },
                )
            }
            else -> onShortcut?.invoke(action, phase)
        }
    }

    /** After a step or tick: the "hold to quit" cue, and the next tick. */
    private fun settle() {
        val armed = slots.values.any { it.engine.armedAction == END_STREAM }
        if (armed != exitArmed) {
            exitArmed = armed
            onExitArmed?.invoke(armed)
        }
        mainHandler.removeCallbacks(tick)
        val due = slots.values.mapNotNull { it.engine.nextDue() }.minOrNull() ?: return
        mainHandler.postDelayed(tick, (due - SystemClock.uptimeMillis()).coerceAtLeast(0))
    }

    /**
     * One joystick MotionEvent — routed to the producing device's own [Gamepad.AxisMapper] (per-device
     * state). Returns true if consumed. Only a real gamepad drives a pad: a DualSense/DS4 motion-sensor
     * sibling node classifies as bare joystick (no GAMEPAD source class) and reports every pad axis as
     * 0, so [isForwardable] filters it out before it can open a slot or clobber axes.
     */
    fun onMotion(event: MotionEvent): Boolean {
        if (!event.isFromSource(InputDevice.SOURCE_JOYSTICK)) return false
        if (event.actionMasked != MotionEvent.ACTION_MOVE) return false
        val dev = event.device ?: return false
        if (!isForwardable(dev)) return false
        val slot = slotFor(dev) ?: return false
        if (ringOpen) {
            // The left stick AIMS: its sector is the slot, so the ring follows the thumb the way
            // a weapon wheel does. Sent on every sector change, neutral included — the D-pad is
            // what steps disc by disc.
            val sector = ringSector(
                event.getAxisValue(MotionEvent.AXIS_X),
                event.getAxisValue(MotionEvent.AXIS_Y),
                stickSector,
            )
            if (sector != stickSector) {
                stickSector = sector
                onRingNav?.invoke(RingNav.Sector(sector))
            }
            return true
        }
        if (forwarding) slot.mapper.onMotion(event)
        return true
    }

    /**
     * The controller currently mapped to wire pad [pad], for feedback routing; null if that index
     * holds no live slot (a pad that just unplugged — the update is then dropped) OR the slot is
     * an [ExternalPad] (its synthetic id resolves to no InputDevice, so rumble binds naturally
     * fall through to the capture link's own feedback path). Read from the feedback poll threads.
     */
    fun deviceForPad(pad: Int): InputDevice? {
        for ((deviceId, slot) in slots) {
            if (slot.index == pad) return InputDevice.getDevice(deviceId)
        }
        return null
    }

    /** Whether ANY live slot currently holds wire pad [pad]. Read from the phone-gyro thread. */
    fun padPresent(pad: Int): Boolean = slots.values.any { it.index == pad }

    /**
     * Whether wire sends are on at all — the forwarding preference AND the session's GAMEPAD
     * grant. For the writers that ride the pad planes from OUTSIDE this router (the phone-gyro
     * mirror), which must stand down with it. Read from the sensor thread.
     */
    fun sendsEnabled(): Boolean = forwarding

    /** Wire pad indices of the open slots, a bit per pad. */
    fun padMask(): Int = slots.values.fold(0) { m, s -> m or (1 shl s.index) }

    /**
     * Whether wire pad [pad]'s motion already comes from the controller's OWN IMU — a capture
     * link's pad (USB DualSense / SC2, opened with `ownMotion`), or a real controller whose gyro
     * [PadSensors] is reading through the platform sensor framework (a Bluetooth DualSense /
     * Switch Pro / 8BitDo). The on-screen pad has none, so the phone's gyro may speak for it.
     * The phone-gyro mirror stands down for the rest: two motion writers on one wire pad would
     * fight, and the pad's own IMU is the one attached to the player's hands.
     * Read from the phone-gyro thread (both tables are concurrent).
     */
    fun padHasOwnMotion(pad: Int): Boolean =
        slots.any { (id, slot) -> slot.index == pad && (slot.ownMotion || id in sensorDevices) }

    /**
     * Declare (or withdraw) that real controller [deviceId] is sourcing its own rotation — see
     * [padHasOwnMotion]. Called by [PadSensors] as it registers and unregisters listeners, on the
     * main thread; read from the phone-gyro thread, hence the concurrent set. Keyed by device
     * rather than by pad index so a controller that changes wire index (a lower one freed up while
     * it was captured) carries the fact with it.
     */
    fun setDeviceHasSensorMotion(deviceId: Int, has: Boolean) {
        if (has) sensorDevices.add(deviceId) else sensorDevices.remove(deviceId)
        // This is the first moment we know a Bluetooth pad actually HAS a gyro — `openSlot` only
        // knows what kind it declared. So it is the honest place to raise the notice when that
        // gyro has nowhere to go, and the only one that cannot nag about a pad that never had one.
        if (has && forwarding && slots[deviceId]?.motionReaches == false) {
            onMotionUnreachable?.invoke()
        }
    }

    /**
     * One motion sample from real controller [deviceId]'s own sensors, on whatever wire index its
     * slot currently holds — [ExternalPad.motion] for pads the input stack still owns. Silently
     * drops when the slot is gone (unplugged, or claimed by a capture link between the sensor
     * callback and here) rather than writing to an index that may already belong to someone else.
     * Called from [PadSensors]' sensor thread.
     */
    fun deviceMotion(deviceId: Int, gyro: IntArray, accel: IntArray) {
        val slot = slots[deviceId] ?: return
        if (!forwarding) return
        // The same gate the USB capture path takes: a backend with no motion plane decodes every
        // sample and discards it, so sending is pure cost. Notified once per pad by
        // [setDeviceHasSensorMotion], which is where we first know the controller HAS a gyro to
        // lose — a pad without one must not produce a warning about motion.
        if (!slot.motionReaches) return
        NativeBridge.nativeSendPadMotion(
            handle, slot.index,
            gyro[0], gyro[1], gyro[2],
            accel[0], accel[1], accel[2],
        )
    }

    /** Snapshot of the REAL controllers currently forwarded, as deviceIds — the set [PadSensors]
     *  sweeps at start for the pads that were already connected when the session opened. */
    fun forwardedDevices(): List<Int> = slots.keys.filter { it >= 0 }

    /**
     * A capture-link pad occupying a wire slot without an Android [InputDevice] — the as-is Steam
     * Controller 2 passthrough (USB/BLE claimed directly, invisible to the input stack). Shares
     * the real slots' lifecycle: a stable lowest-free index, Arrival-before-input, held-state
     * flush + Remove on [close], and its own shortcuts engine.
     */
    inner class ExternalPad internal constructor(
        private val syntheticId: Int,
        val index: Int,
        /**
         * Whether this pad's motion can reach the game at all, asked once at open (see
         * [NativeBridge.nativePadMotionReaches]). False means the host built this pad a backend
         * without a motion plane, so [motion] drops the sample here instead of paying to send one
         * the host will decode and discard — at a controller's full report rate, for the whole
         * session.
         */
        private val motionReaches: Boolean,
    ) {
        // Live lookup instead of a captured reference: after [close] (or a router release) the
        // slot is gone from the table and every entry point below degrades to a safe no-op.
        private val slot get() = slots[syntheticId]

        /** One button transition (a wire [Gamepad].BTN_* bit). On-change only — the caller diffs. */
        fun button(bit: Int, down: Boolean) {
            slot?.let { slotButton(it, bit, down, send = true) }
        }

        /**
         * One axis update ([Gamepad].AXIS_*: stick i16 +y=up / trigger 0..255). On-change only.
         * Dropped while the ring owns the pad, like a button: [setRingOpen] zeroed the axes on
         * the wire, and a stick still held under the ring must not write over that.
         */
        fun axis(id: Int, value: Int) {
            if (slot != null && forwarding && !ringOpen) NativeBridge.nativeSendGamepadAxis(handle, id, value, index)
        }

        /** The pad's identity, for the host's virtual pad ([NativeBridge.nativeSendPadIdentity]). */
        fun identity(puckSlot: Int, serial: String, replies: ByteArray) {
            if (slot != null && forwarding) {
                NativeBridge.nativeSendPadIdentity(handle, index, puckSlot, serial, replies)
            }
        }

        /** One raw HID report, forwarded verbatim for the host's as-is virtual pad. */
        fun hidReport(buf: java.nio.ByteBuffer, len: Int) {
            if (slot != null && forwarding) NativeBridge.nativeSendPadHidReport(handle, index, buf, len)
        }

        /** One touchpad contact on the rich plane: [finger] 0/1, x/y normalized 0..65535 in
         *  SCREEN convention (+y down); `active = false` lifts the finger. On-change only. */
        fun touch(finger: Int, active: Boolean, x: Int, y: Int) {
            if (slot != null && forwarding) {
                NativeBridge.nativeSendPadTouch(handle, index, finger, active, x, y)
            }
        }

        /** One motion sample on the rich plane (gyro pitch/yaw/roll + accel, raw device i16
         *  units — the host passes them straight into the virtual pad's report). Per report. */
        fun motion(gyro: IntArray, accel: IntArray) {
            if (slot != null && forwarding && motionReaches) {
                NativeBridge.nativeSendPadMotion(
                    handle, index,
                    gyro[0], gyro[1], gyro[2],
                    accel[0], accel[1], accel[2],
                )
            }
        }

        /** Flush held state, signal the removal, and free the wire index. Idempotent. */
        fun close() = closeSlot(syntheticId)
    }

    /**
     * Open a slot for a capture-link pad, declaring [pref] as its kind; null when all 16 wire
     * indices are taken. Main thread (like the hot-plug callbacks).
     *
     * [hasGyro] says whether this link forwards motion on the RICH plane ([ExternalPad.motion]) —
     * true for the Sony pads, whose IMU is a headline feature, and false for the Steam Controller 2,
     * whose motion rides inside the opaque passthrough report that [ExternalPad.hidReport] carries
     * and which nothing here may second-guess. It gates only the notice: a pad that never sends
     * motion must not produce a warning about motion.
     *
     * [ownMotion] says whether the pad has an IMU at all ([padHasOwnMotion]): true for every
     * capture link (the Steam Controller 2's rides in its passthrough report), false only for
     * the on-screen pad, which the phone's own gyro may then drive.
     */
    fun openExternal(pref: Int, hasGyro: Boolean = false, ownMotion: Boolean = true): ExternalPad? {
        val index = lowestFreeIndex() ?: return null
        // Synthetic ids live below any real InputDevice id (those are positive), so they can't
        // collide and InputDevice.getDevice(id) resolves them to null for the feedback path.
        val syntheticId = EXTERNAL_ID_BASE - index
        if (forwarding) NativeBridge.nativeSendGamepadArrival(handle, pref, index)
        // Asked once, here, off the kind this pad just DECLARED — not off the session's resolved
        // backend, which under Automatic answers for whichever pad happened to be active at dial
        // time. Cheap enough to ask unconditionally; the answer holds for the pad's lifetime.
        val motionReaches = NativeBridge.nativePadMotionReaches(handle, pref)
        if (forwarding && hasGyro && !motionReaches) onMotionUnreachable?.invoke()
        // `DsDevice` raises BTN_MISC1 from the DualSense report's mute bit; `Sc2Device` raises the
        // same bit from the Steam Controller 2's QAM button, which must not touch the microphone.
        // The declared kind separates them (a DualShock 4 has no mute button either).
        val hasMute = pref == Gamepad.PREF_DUALSENSE || pref == Gamepad.PREF_DUALSENSEEDGE
        slots[syntheticId] = Slot(
            index,
            Gamepad.AxisMapper(handle, index),
            hasMuteButton = hasMute,
            ownMotion = ownMotion,
            engine = newEngine(),
        )
        pushSc2Gate()
        return ExternalPad(syntheticId, index, motionReaches)
    }

    /**
     * Close the slot (if any) for a physical controller a capture link just claimed. The claim
     * detaches the kernel driver, so the system's own removal callback would close it moments
     * later anyway — doing it at claim time makes the freed wire index deterministic for the
     * link's [ExternalPad] instead of racing the link's first report against that callback. Safe
     * to over-match (a same-VID/PID sibling that still exists as an InputDevice lazily reopens a
     * slot on its next input event). Main thread, like the hot-plug callbacks.
     */
    fun releaseDevice(deviceId: Int) = closeSlot(deviceId)

    /**
     * Emulate [pref] for the rest of this stream. The host builds a pad from its Arrival and never
     * swaps a built one, so each controller whose declared kind moves is closed and reopened. The
     * capture links declare their own kind and are left alone. Main thread.
     */
    fun setPadType(pref: Int) {
        padType = pref
        for (id in forwardedDevices()) {
            val dev = InputDevice.getDevice(id) ?: continue
            if (slots[id]?.declared == declaredFor(dev)) continue
            closeSlot(id)
            openSlot(dev)
        }
    }

    /** Automatic declares the pad's own type from its VID/PID; an explicit type covers every pad. */
    private fun declaredFor(dev: InputDevice): Int =
        if (padType == Gamepad.PREF_AUTO) Gamepad.prefFor(dev) else padType

    /**
     * Flush + drop every slot and unregister the hot-plug listener. Call on session teardown, AFTER
     * the feedback poll threads are joined (they read [deviceForPad]).
     */
    fun release() {
        inputManager?.unregisterInputDeviceListener(listener)
        // Snapshot the ids first — closeSlot mutates the map.
        for (id in slots.keys.toList()) closeSlot(id)
        mainHandler.removeCallbacks(tick) // no hold fires after teardown
    }

    // ---- slots ----

    /** A real, non-virtual controller we forward — its source classes include GAMEPAD (excludes a pad's bare-joystick sensor node). */
    private fun isForwardable(dev: InputDevice): Boolean =
        !dev.isVirtual && dev.sources and InputDevice.SOURCE_GAMEPAD == InputDevice.SOURCE_GAMEPAD

    /**
     * The slot for [dev], opening one (and declaring the pad) if this device is unseen; null when [dev]
     * isn't a forwardable controller or every wire index is taken. The [isForwardable] gate lives here —
     * the single lazy-open chokepoint both [onButton] and [onMotion] funnel through — so no entry point
     * can open a phantom slot for a virtual/non-gamepad source (the hot-plug listener and init loop
     * pre-filter and call [openSlot] directly).
     */
    private fun slotFor(dev: InputDevice?): Slot? {
        if (dev == null) return null
        slots[dev.id]?.let { return it }
        if (!isForwardable(dev)) return null
        return openSlot(dev)
    }

    /**
     * Open a slot for [dev] on the lowest free wire index, declaring its kind ([NativeBridge.nativeSendGamepadArrival])
     * before any input so the host builds a matching virtual device (mixed types across pads).
     * Idempotent; null when all 16 wire indices are already forwarded.
     */
    private fun openSlot(dev: InputDevice): Slot? {
        slots[dev.id]?.let { return it }
        val index = lowestFreeIndex() ?: return null // 16 pads already forwarded — drop this one
        val pref = declaredFor(dev)
        if (forwarding) NativeBridge.nativeSendGamepadArrival(handle, pref, index)
        // Asked here, off the kind this pad just DECLARED — not off the session's resolved backend,
        // which under Automatic answers for whichever pad happened to be active at dial time. Held
        // for the slot's life; the sensor path reads it on every sample.
        val map = Gamepad.padMap(dev)
        val slot = Slot(
            index,
            Gamepad.AxisMapper(handle, index, map),
            NativeBridge.nativePadMotionReaches(handle, pref),
            // The only route to BTN_MISC1 on this path is GENERIC_SONY's `0x13e` row, so the
            // report order IS the answer — and unlike `pref` it survives the user pinning every
            // pad to one type, which would otherwise cost a DualSense its mute button.
            hasMuteButton = map.buttons == Gamepad.PadButtons.GENERIC_SONY,
            declared = pref,
            engine = newEngine(),
        )
        slots[dev.id] = slot
        // After the table holds the slot, so a listener that sends on this device the moment it is
        // told ([PadSensors]) finds an index to send on rather than dropping its first samples.
        onSlotOpened?.invoke(dev.id)
        return slot
    }

    /**
     * Flush a slot's held wire state (so nothing sticks host-side), signal the removal, and free its
     * index. Safe against an already-gone device — the flush emits wire events only, no device access.
     */
    private fun closeSlot(deviceId: Int) {
        val slot = slots.remove(deviceId) ?: return
        // A held Guide or key combination comes up, an owed tap release goes out now.
        apply(slot, slot.engine.flush())
        releaseHeld(slot)
        if (forwarding) NativeBridge.nativeSendGamepadRemove(handle, slot.index)
        settle()
        // Release this controller's feedback bindings (close its lights session / cancel rumble).
        onSlotClosed?.invoke(deviceId)
    }

    /** A new slot's engine. Opening the ring counts only while something opens it. */
    private fun newEngine() = Shortcuts.Engine(
        shortcuts.filter { it.action != Shortcuts.Action.Ring || onRingChord != null },
        Shortcuts.Device.PAD,
    )

    /** Lift every held button + zero the axes/HAT dpad for [slot] (wire events only, all on its index). */
    private fun releaseHeld(slot: Slot) {
        var bits = slot.held
        while (bits != 0) {
            val bit = bits and -bits // lowest set bit
            if (forwarding && !localOnly(slot, bit)) {
                NativeBridge.nativeSendGamepadButton(handle, bit, false, slot.index)
            }
            bits = bits and bit.inv()
        }
        slot.held = 0
        if (forwarding) slot.mapper.reset() // zero sticks/triggers + release the HAT dpad
    }

    /** Lowest wire index 0..[MAX_PADS) not held by a slot, or null when full — stable lowest-free keeps indices from shuffling on hot-plug. */
    private fun lowestFreeIndex(): Int? {
        val taken = slots.values.mapTo(HashSet()) { it.index }
        for (i in 0 until MAX_PADS) if (i !in taken) return i
        return null
    }

    // `internal` rather than private: the pure helpers are the only part of this router a JVM
    // unit test can reach; everything else needs an InputManager, a main Looper and live
    // InputDevices. Still invisible to :app, which is what private bought.
    internal companion object {
        /** Mirror of `punktfunk-core::input::MAX_PADS` — wire pad indices 0..15. */
        const val MAX_PADS = 16

        /** The C ABI's `PUNKTFUNK_SC2_GATE_*` bits, for [NativeBridge.nativeSetSc2Gate]. */
        const val SC2_GATE_MASKED = 1
        const val SC2_GATE_SYSTEM_LOCAL = 2
        const val SC2_GATE_CHORDS = 4

        /** A sector, once engaged, keeps the stick until the angle is this far past its 30° edge —
         *  a thumb resting between two slots would otherwise flicker between them. */
        const val SECTOR_OVERLAP_DEG = 5.0

        /**
         * The ring slot the left stick points at, given the sector already engaged: past the dead
         * zone by MAGNITUDE (a diagonal counts) the angle falls into one of six 60° sectors centred
         * on the slots, slot `k` at `-90° + 60°·k`, 12 o'clock first, clockwise. The Kotlin half of
         * `pf_client_core::menu_nav::ring_sector` — same 0.5 engage / 0.3 release thresholds, so
         * the dial feels identical on a phone and on a Steam Deck. [MotionEvent.AXIS_Y] is +down,
         * which is already the screen's own sense.
         */
        fun ringSector(x: Float, y: Float, current: Int?): Int? {
            if (hypot(x, y) <= (if (current == null) 0.5f else 0.3f)) return null
            // Degrees clockwise from 12 o'clock, so slot k's centre is at 60·k. atan2 spans
            // (-180°, 180°], so the +90 turn can only reach -90 — one wrap covers it.
            var deg = Math.toDegrees(atan2(y.toDouble(), x.toDouble())) + 90.0
            if (deg < 0) deg += 360.0
            if (current != null) {
                // Signed distance from the engaged slot's centre, folded into ±180°.
                val off = (deg - 60.0 * current + 540.0) % 360.0 - 180.0
                if (abs(off) <= 30.0 + SECTOR_OVERLAP_DEG) return current
            }
            return ((deg + 30.0) / 60.0).toInt() % 6
        }

        private val END_STREAM = Shortcuts.Action.Slot(SlotId.EndStream)

        /**
         * A synthetic tap's wire events as (bit, down, after ms) — pf-client-core's
         * `tap_events`: each press [Shortcuts.TAP_PRESS_MS] after the one before, then every
         * release together, last pressed first. Apart, because core folds per-transition sends
         * into one state, and because a chord's first button has to be held before its second
         * counts. A [android.os.Handler] runs equal delays in the order they were posted.
         */
        internal fun tapEvents(presses: List<Int>): List<Triple<Int, Boolean, Long>> {
            val downs = presses.mapIndexed { i, bit -> Triple(bit, true, i * Shortcuts.TAP_PRESS_MS) }
            val up = presses.size * Shortcuts.TAP_PRESS_MS
            return downs + presses.reversed().map { Triple(it, false, up) }
        }

        /**
         * Whether a key from a device with no slot yet opens one: a `KEYCODE_BUTTON_*` key, or any
         * pad key from a device with a stick. A TV remote's D-pad, OK and Back stay keys.
         */
        internal fun opensSlotOnKey(gamepadButton: Boolean, hasStick: Boolean): Boolean =
            gamepadButton || hasStick

        /** Synthetic slot-key base for [ExternalPad]s — below every real (positive) InputDevice id. */
        const val EXTERNAL_ID_BASE = -1000
    }
}
