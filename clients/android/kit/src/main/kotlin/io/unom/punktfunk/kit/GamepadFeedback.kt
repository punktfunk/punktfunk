package io.unom.punktfunk.kit

import android.content.Context
import android.graphics.Color
import android.hardware.lights.Light
import android.hardware.lights.LightState
import android.hardware.lights.LightsManager
import android.hardware.lights.LightsRequest
import android.os.Build
import android.os.CombinedVibration
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.util.Log
import android.view.InputDevice
import java.nio.ByteBuffer

/**
 * Host→client gamepad feedback for one session, routed per controller by wire pad index. Two daemon
 * poll threads drain the blocking native pulls and render in Kotlin: rumble → the addressed
 * controller's `VibratorManager` (API 31+) or its single legacy `Vibrator` on API 28–30; HID-output
 * → that controller's lightbar / player-LED via `LightsManager` (API 33+); adaptive triggers are
 * parse-validated and logged (Android has no public adaptive-trigger API).
 *
 * Each pull carries the wire pad index it is addressed to; [GamepadRouter.deviceForPad] resolves it
 * to the physical controller currently holding that index — so a rumble the host aimed at pad 1
 * drives pad 1's motors, and an update for an index with no live controller (a pad that just
 * unplugged) is dropped. Per-controller rumble/light bindings are built lazily and cached by device
 * id (bounded — at most 16 pads).
 *
 * Mirrors `nativeStartAudio`'s lifecycle: [start]/[stop] driven by the StreamScreen. [stop] flips a
 * flag; the ~100 ms native pull timeout lets the threads exit, then they're joined (bounded) — and
 * this MUST run before the router is released and `nativeClose` frees the session handle.
 *
 * With no controller connected (emulator) rumble/lights become logged no-ops — exactly the
 * verification path; the `Log.i` receipt lines fire regardless of rendering hardware.
 *
 * [bodyVibrator] is this device's own motor ([deviceBodyVibrator]). It plays a controller's rumble
 * when [bodyRumbleFor] says the body stands in for that pad. With [mirrorPad0] ("Rumble on this
 * phone", off by default) it also plays every rumble addressed to wire pad 0 (controller 1) — for
 * clip-on gamepads that ship without rumble motors.
 */
class GamepadFeedback(
    private val handle: Long,
    private val router: GamepadRouter?,
    private val bodyVibrator: Vibrator? = null,
    private val mirrorPad0: Boolean = false,
) {
    /**
     * A capture link's feedback renderer for the wire pads it owns, consulted BEFORE the
     * InputDevice vibrator/lights paths. A captured controller has no [android.view.InputDevice]
     * (its slot is an [GamepadRouter.ExternalPad] on a synthetic id, so [GamepadRouter.deviceForPad]
     * resolves null and the platform paths no-op) — the link renders instead, by composing USB
     * output reports on the physical pad. This is also the ONLY route to adaptive triggers:
     * Android has no platform API for them, so without a sink a Trigger event is log-and-drop.
     * Invoked on the feedback poll threads; implementations must be thread-safe.
     */
    interface PadFeedbackSink {
        /** True when this sink renders feedback for wire pad [pad]; the render methods are only
         *  invoked while true. Racing a pad close is fine — a late render is a harmless no-op. */
        fun ownsPad(pad: Int): Boolean

        /** One effective rumble command (`(0,0)` = stop now; else a one-shot at this level with
         *  [backstopMs] as the self-termination net — see [GamepadFeedback.renderRumble]). */
        fun rumble(pad: Int, low: Int, high: Int, backstopMs: Long)

        /** Lightbar RGB. */
        fun led(pad: Int, r: Int, g: Int, b: Int)

        /** Player-indicator LED bitmask (low 5 bits, hid-playstation layout). */
        fun playerLeds(pad: Int, bits: Int)

        /** Mic-mute LED: [mode] 0 off, 1 on, 2 pulse. A pad without one ignores it. */
        fun micLed(pad: Int, mode: Int) {}

        /** One adaptive-trigger effect: [which] 0 = L2, 1 = R2; [effect] = the raw DS5 trigger
         *  block (mode byte + parameters) exactly as the game wrote it host-side. */
        fun trigger(pad: Int, which: Int, effect: ByteArray)
    }

    /**
     * The active capture link's sink (a [DsCapture]), or null. Wired by StreamScreen alongside
     * [onHidRaw]; cleared before the poll threads stop.
     */
    @Volatile
    var sink: PadFeedbackSink? = null

    private companion object {
        const val TAG = "pf.feedback"
        const val TAG_LED: Byte = 0x01
        const val TAG_PLAYER_LEDS: Byte = 0x02
        const val TAG_TRIGGER: Byte = 0x03
        const val TAG_HID_RAW: Byte = 0x05
        const val TAG_MIC_LED: Byte = 0x07

        /** Sparse-log cadence for swallowed render failures — see [noteRenderFailure]. */
        const val LOG_EVERY = 128L
    }

    /**
     * One controller's rumble binding — VibratorManager (API 31+) OR one legacy Vibrator: the
     * pad's own on API 28–30, or the device body's ([bodyRumbleFor]).
     */
    private class RumbleBind(
        val vm: VibratorManager?,
        val legacy: Vibrator?,
        val ids: IntArray,
        val amplitudeControlled: Boolean,
    )

    /** One controller's lights binding (API 33+): its open session + the RGB / player-id lights it exposes. */
    private class LightBind(
        val session: LightsManager.LightsSession,
        val rgb: Light?,
        val player: Light?,
    )

    @Volatile private var running = false
    private var rumbleThread: Thread? = null
    private var hidoutThread: Thread? = null

    // Per-controller bindings, keyed by device id, built lazily. rumbleBinds is written by the rumble
    // thread and lightBinds by the hidout thread while running; [onDeviceRemoved] also evicts+closes
    // from the MAIN thread on a hot-unplug, and stop() clears both from the main thread after joining
    // the threads. That main-vs-poll concurrency is why every access goes through `bindsLock` (a plain
    // HashMap can corrupt under a concurrent structural write, and ConcurrentHashMap can't hold the
    // null value that caches "this controller has no vibrator / no controllable lights"). The lock
    // guards only the map ops — rendering runs on the returned reference outside it; a stale reference
    // is harmless (a closed LightsSession's requestLights and a cancelled Vibrator are runCatching'd
    // no-ops). A null value caches the negative result so a pad with no hardware isn't re-probed.
    private val bindsLock = Any()
    private val rumbleBinds = HashMap<Int, RumbleBind?>()
    private val lightBinds = HashMap<Int, LightBind?>()

    fun start() {
        running = true
        rumbleThread = Thread({
            var failures = 0L
            while (running) {
                val ev = NativeBridge.nativeNextRumble(handle)
                // Layout + semantics live in `unpackRumbleEvent` (RumbleWire.kt), tested there
                // against the Rust packer.
                val cmd = unpackRumbleEvent(ev) ?: continue // timeout / closed
                // Rendering is binder calls into the vibrator service, and every one of them can
                // throw unchecked — DeadSystemRuntimeException when system_server goes down, and
                // the ordinary RuntimeException a dying service wraps its RemoteException in.
                // Unguarded, ONE of those killed this thread outright: `running` stayed true, so
                // nothing noticed and nothing restarted it, and rumble was gone for the rest of
                // the session. Losing a single command is recoverable; losing the loop is not.
                runCatching {
                    renderRumble(cmd.pad, cmd.low, cmd.high, cmd.backstopMs)
                }.onFailure { failures = noteRenderFailure("rumble", it, failures) }
            }
        }, "pf-rumble").apply { isDaemon = true; start() }

        hidoutThread = Thread({
            // 128: the raw as-is passthrough events are [pad][kind tag][report kind][≤64 bytes].
            val buf = ByteBuffer.allocateDirect(128)
            var failures = 0L
            while (running) {
                val n = NativeBridge.nativeNextHidout(handle, buf)
                if (n < 0) continue // timeout / closed
                // Same hazard as the rumble loop above: lights/trigger rendering is binder and USB
                // calls, and an unchecked throw here would silently end the rich-feedback plane.
                runCatching { dispatchHidout(buf, n) }
                    .onFailure { failures = noteRenderFailure("hidout", it, failures) }
            }
        }, "pf-hidout").apply { isDaemon = true; start() }
    }

    /**
     * Record a render failure the poll loop swallowed, and return the updated count. Logged on the
     * first occurrence and sparsely after: a genuinely dead vibrator service fails on *every*
     * command, which at a rumble plane's rate would bury the log.
     */
    private fun noteRenderFailure(plane: String, t: Throwable, seen: Long): Long {
        if (seen == 0L || seen % LOG_EVERY == 0L) {
            Log.w(TAG, "$plane render failed (#${seen + 1}) — command dropped, poll loop alive", t)
        }
        return seen + 1
    }

    /** Idempotent. Stops + joins the poll threads (must complete before the router is released / handle freed). */
    fun stop() {
        running = false
        rumbleThread?.interrupt()
        hidoutThread?.interrupt()
        // Join WITHOUT a timeout. These poll threads dereference the native session handle on every
        // pull (nativeNextRumble/nativeNextHidout) and read the router, so they MUST be dead before
        // StreamScreen's onDispose reaches router.release() / nativeClose, which free that state. A
        // *bounded* join that times out would let a thread survive into the freed handle → use-after-
        // free SIGSEGV (the back-while-streaming crash, on the one path the main-thread `closed` guard
        // can't cover). Safe to block unbounded: the native pulls are internally time-bounded
        // (PULL_TIMEOUT ~100 ms) and rendering is a quick best-effort binder call, so each thread
        // observes running=false and exits within ~one timeout — the join returns promptly.
        runCatching { rumbleThread?.join() }
        runCatching { hidoutThread?.join() }
        rumbleThread = null
        hidoutThread = null
        // Threads are dead — drop any held rumble (incl. the device body's) and close every
        // lights session.
        runCatching { bodyVibrator?.cancel() }
        synchronized(bindsLock) {
            for (b in rumbleBinds.values) b?.let {
                runCatching { it.vm?.cancel() }
                runCatching { it.legacy?.cancel() }
            }
            for (b in lightBinds.values) b?.let { runCatching { it.session.close() } }
            rumbleBinds.clear()
            lightBinds.clear()
        }
    }

    /**
     * Evict and release the bindings for a controller that just disconnected — invoked from
     * [GamepadRouter]'s slot-close on the main thread (routed via `StreamScreen`). Closes its
     * `LightsSession` and cancels any held rumble, so a hot-unplug mid-session frees the session
     * immediately instead of leaking it until [stop]. A no-op for a device with no cached binding.
     * The next feedback for that pad index rebinds against whatever controller now holds it.
     */
    // Same runtime-guarded cleanup as [stop] (VIBRATE is app-declared; the light bind only exists
    // under the SDK 33 guard) — suppress the module-isolation lint false positives it re-triggers.
    @Suppress("MissingPermission", "NewApi")
    fun onDeviceRemoved(deviceId: Int) {
        synchronized(bindsLock) {
            rumbleBinds.remove(deviceId)?.let {
                runCatching { it.vm?.cancel() }
                runCatching { it.legacy?.cancel() }
            }
            lightBinds.remove(deviceId)?.let { runCatching { it.session.close() } }
        }
    }

    // ---- Rumble ----

    /** The rumble binding for the controller on wire pad [pad], or null (no live pad / no vibrator). Cached by device id. */
    private fun rumbleBindFor(pad: Int): RumbleBind? {
        val dev = router?.deviceForPad(pad) ?: return null
        synchronized(bindsLock) {
            if (rumbleBinds.containsKey(dev.id)) return rumbleBinds[dev.id]
            val bind = bindRumble(dev)
            rumbleBinds[dev.id] = bind
            return bind
        }
    }

    private fun bindRumble(dev: InputDevice): RumbleBind? {
        // The body is one actuator, so it renders as a single legacy motor.
        bodyRumbleFor(dev, bodyVibrator)?.let {
            Log.i(TAG, "rumble: built-in controller '${dev.name}' has no vibrators — bound the device body")
            return RumbleBind(null, it, IntArray(0), it.hasAmplitudeControl())
        }
        if (Build.VERSION.SDK_INT >= 31) {
            val m = dev.vibratorManager
            val ids = m.vibratorIds
            if (ids.isEmpty()) {
                Log.i(TAG, "rumble: controller '${dev.name}' has no vibrators — rumble no-op")
                return null
            }
            val amp = ids.all { m.getVibrator(it).hasAmplitudeControl() }
            Log.i(TAG, "rumble: bound ${ids.size} vibrators for '${dev.name}' amplitudeControl=$amp")
            return RumbleBind(m, null, ids, amp)
        }
        // API 28–30: no VibratorManager — fall back to the controller's single legacy Vibrator.
        @Suppress("DEPRECATION")
        val v = dev.vibrator
        if (!v.hasVibrator()) {
            Log.i(TAG, "rumble: controller '${dev.name}' has no vibrator — rumble no-op")
            return null
        }
        Log.i(TAG, "rumble: bound legacy vibrator for '${dev.name}' amplitudeControl=${v.hasAmplitudeControl()}")
        return RumbleBind(null, v, IntArray(0), v.hasAmplitudeControl())
    }

    /**
     * low = heavy/left motor, high = light/right motor; both 0..0xFFFF (the host's u16 amplitudes),
     * addressed to wire pad [pad]. `durationMs` is the engine command's backstop — the one-shot's
     * self-termination net under a stalled poll thread; the engine emits explicit zero commands at
     * every policy stop (lease expiry, legacy staleness, session close), so cancel-on-zero is the
     * real stop mechanism.
     */
    private fun renderRumble(pad: Int, low: Int, high: Int, durationMs: Long) {
        Log.i(TAG, "rumble pad=$pad low=$low high=$high backstopMs=$durationMs") // verification line — BEFORE any no-op return
        // The opt-in phone mirror plays before the capture and bind returns below: it follows
        // controller 1 whatever that pad is, because the player opted in.
        val mirrored = pad == 0 && mirrorPad0
        if (mirrored) renderDeviceRumble(low, high, durationMs)
        // A captured pad's link renders on the physical controller itself (its slot has no
        // InputDevice, so the vibrator bind below would resolve null and drop the command).
        sink?.takeIf { it.ownsPad(pad) }?.let {
            it.rumble(pad, low, high, durationMs)
            return
        }
        val bind = rumbleBindFor(pad) ?: return
        if (mirrored && bind.legacy === bodyVibrator) return // the mirror already drove the body
        val lo = wireAmplitudeToByte(low)
        val hi = wireAmplitudeToByte(high)
        val m = bind.vm
        if (m != null) {
            if (lo == 0 && hi == 0) {
                runCatching { m.cancel() } // (0,0) = stop
                return
            }
            val combo = CombinedVibration.startParallel()
            if (bind.amplitudeControlled && bind.ids.size >= 2) {
                // Two-motor split — ASSUMPTION: ids[0] = light/right, ids[1] = heavy/left
                // (XInput/Moonlight convention). Android does not guarantee the order of
                // VibratorManager.getVibratorIds(), so a pad that enumerates heavy-first would
                // invert the feel: the stronger amplitude drives the physically-lighter motor.
                // Failure mode is tactile only — both motors still fire, nothing silences or
                // crashes — so this stays the default pending per-pad on-glass verification (G20).
                // ids beyond the first two (rare) are left alone here.
                if (hi != 0) combo.addVibrator(bind.ids[0], oneShot(hi, durationMs))
                if (lo != 0) combo.addVibrator(bind.ids[1], oneShot(lo, durationMs))
            } else {
                // Single motor or no amplitude control: blend both into one effect.
                val a = (lo * 0.8 + hi * 0.33).toInt().coerceIn(1, 255)
                for (id in bind.ids) combo.addVibrator(id, oneShot(a, durationMs))
            }
            runCatching { m.vibrate(combo.combine()) }
            return
        }
        // API 28–30 legacy single-motor path: blend both motors into one effect.
        val lv = bind.legacy ?: return
        if (lo == 0 && hi == 0) {
            runCatching { lv.cancel() } // (0,0) = stop
            return
        }
        val a = (lo * 0.8 + hi * 0.33).toInt().coerceIn(1, 255)
        runCatching {
            lv.vibrate(
                if (bind.amplitudeControlled) oneShot(a, durationMs)
                else oneShot(VibrationEffect.DEFAULT_AMPLITUDE, durationMs)
            )
        }
    }

    /**
     * The opt-in phone mirror: play a wire-pad-0 rumble on [bodyVibrator] —
     * one physical actuator, so both wire motors blend into one effect (the same blend as the
     * single-motor controller path). Same envelope semantics too: a one-shot held for the host's
     * TTL, cancel on (0,0).
     */
    private fun renderDeviceRumble(low: Int, high: Int, durationMs: Long) {
        val v = bodyVibrator ?: return
        val lo = wireAmplitudeToByte(low)
        val hi = wireAmplitudeToByte(high)
        if (lo == 0 && hi == 0) {
            runCatching { v.cancel() } // (0,0) = stop
            return
        }
        val a = (lo * 0.8 + hi * 0.33).toInt().coerceIn(1, 255)
        runCatching {
            v.vibrate(
                if (v.hasAmplitudeControl()) oneShot(a, durationMs)
                else oneShot(VibrationEffect.DEFAULT_AMPLITUDE, durationMs)
            )
        }
    }

    // One-shot held for `durationMs` — the host's v2 TTL (renewed while the level holds), so it
    // self-terminates on a lost stop; cancel on zero. Floor the duration at 1 ms: `createOneShot`
    // throws IllegalArgumentException on a non-positive duration, and a lease can carry ttl_ms==0
    // (e.g. the legacy-Deck ceiling) with a nonzero amplitude — which reaches here past the (0,0)
    // stop guard. On the VibratorManager path the effect is built OUTSIDE the vibrate() runCatching,
    // so an uncaught throw here would kill the whole rumble poll thread.
    private fun oneShot(amp: Int, durationMs: Long): VibrationEffect =
        VibrationEffect.createOneShot(durationMs.coerceAtLeast(1), amp)

    // ---- HID output ----

    private fun dispatchHidout(buf: ByteBuffer, n: Int) {
        buf.rewind()
        val pad = buf.get().toInt() and 0xFF // wire pad index the event is addressed to
        when (buf.get()) { // kind tag
            TAG_LED -> {
                val r = buf.get().toInt() and 0xFF
                val g = buf.get().toInt() and 0xFF
                val b = buf.get().toInt() and 0xFF
                Log.i(TAG, "hidout pad=$pad Led r=$r g=$g b=$b") // verification line
                val s = sink?.takeIf { it.ownsPad(pad) }
                if (s != null) s.led(pad, r, g, b)
                else if (Build.VERSION.SDK_INT >= 33) setLightbar(pad, Color.rgb(r, g, b))
            }
            TAG_PLAYER_LEDS -> {
                val bits = buf.get().toInt() and 0x1F
                val player = playerIndexForBits(bits)
                Log.i(TAG, "hidout pad=$pad PlayerLeds bits=$bits player=$player") // verification line
                val s = sink?.takeIf { it.ownsPad(pad) }
                if (s != null) s.playerLeds(pad, bits)
                else if (Build.VERSION.SDK_INT >= 33) setPlayerId(pad, player)
            }
            TAG_MIC_LED -> {
                // No platform API lights it: only a captured DualSense can.
                val mode = buf.get().toInt() and 0xFF
                sink?.takeIf { it.ownsPad(pad) }?.micLed(pad, mode)
            }
            TAG_TRIGGER -> {
                val which = buf.get().toInt() and 0xFF // 0 = L2, 1 = R2
                val effLen = n - 3 // [pad][kind][which] header, then the effect block
                val s = sink?.takeIf { it.ownsPad(pad) }
                if (s != null && effLen > 0) {
                    // A captured DualSense: the raw trigger block replays onto the physical pad.
                    val effect = ByteArray(effLen)
                    buf.get(effect)
                    Log.i(TAG, "hidout pad=$pad Trigger which=$which effLen=$effLen → captured pad") // verification line
                    s.trigger(pad, which, effect)
                } else {
                    val mode = if (effLen > 0) buf.get().toInt() and 0xFF else 0
                    // No platform adaptive-trigger API — parse-validate the mode + log only.
                    Log.i(
                        TAG,
                        "hidout pad=$pad Trigger which=$which effLen=$effLen mode=0x%02x (no adaptive-trigger renderer for this pad)".format(mode),
                    )
                }
            }
            TAG_HID_RAW -> {
                // As-is SC2 passthrough: a raw report the host's Steam wrote to the virtual pad —
                // [kind: 0=output, 1=feature][report bytes, id first]. Handed to the capture link
                // for verbatim replay on the physical controller; dropped when no link owns the pad.
                val kind = buf.get().toInt() and 0xFF
                val len = n - 3
                if (len > 0) {
                    val data = ByteArray(len)
                    buf.get(data)
                    onHidRaw?.invoke(pad, kind, data)
                }
            }
            else -> Log.d(TAG, "hidout: unknown kind, dropped")
        }
    }

    /**
     * Raw HID-report replay hook for the as-is Steam Controller 2 passthrough: invoked (on the
     * hidout poll thread) with the wire pad index, the report kind (0 = output report, 1 =
     * feature report), and the full report bytes (id first) the host's hidraw consumer wrote.
     * `StreamScreen` wires this to the SC2 capture so Steam's rumble/settings land on the
     * physical controller.
     */
    @Volatile
    var onHidRaw: ((pad: Int, kind: Int, data: ByteArray) -> Unit)? = null

    /** hid-playstation 5-LED pattern → player index 1..4 (0 = off); falls back to a bit count. */
    private fun playerIndexForBits(bits: Int): Int = when (bits and 0x1F) {
        0b00000 -> 0
        0b00100 -> 1
        0b01010 -> 2
        0b10101 -> 3
        0b11011 -> 4
        else -> Integer.bitCount(bits and 0x1F).coerceIn(1, 4)
    }

    /** The lights binding for the controller on wire pad [pad], or null (no live pad / no lights / < API 33). Cached by device id. */
    private fun lightBindFor(pad: Int): LightBind? {
        if (Build.VERSION.SDK_INT < 33) return null
        val dev = router?.deviceForPad(pad) ?: return null
        synchronized(bindsLock) {
            if (lightBinds.containsKey(dev.id)) return lightBinds[dev.id]
            val bind = bindLights(dev)
            lightBinds[dev.id] = bind
            return bind
        }
    }

    private fun bindLights(dev: InputDevice): LightBind? {
        val lm = dev.lightsManager
        var rgb: Light? = null
        var player: Light? = null
        for (l in lm.lights) {
            if (rgb == null && l.hasRgbControl()) rgb = l
            if (player == null && l.type == Light.LIGHT_TYPE_PLAYER_ID) player = l
        }
        if (rgb == null && player == null) {
            Log.i(TAG, "lights: controller '${dev.name}' exposes no controllable lights — no-op")
            return null
        }
        val session = lm.openSession()
        Log.i(TAG, "lights: bound rgb=${rgb != null} playerLed=${player != null} for '${dev.name}'")
        return LightBind(session, rgb, player)
    }

    private fun setLightbar(pad: Int, argb: Int) {
        val bind = lightBindFor(pad) ?: return
        val l = bind.rgb ?: return
        runCatching {
            bind.session.requestLights(LightsRequest.Builder().addLight(l, LightState.Builder().setColor(argb).build()).build())
        }
    }

    private fun setPlayerId(pad: Int, player: Int) {
        val bind = lightBindFor(pad) ?: return
        val l = bind.player ?: return
        runCatching {
            bind.session.requestLights(LightsRequest.Builder().addLight(l, LightState.Builder().setPlayerId(player).build()).build())
        }
    }
}

/**
 * This device's own body vibrator (the phone, not a controller), or null where there is none
 * (TVs) — gates the "Rumble on this phone" setting's visibility and feeds [GamepadFeedback].
 */
fun deviceBodyVibrator(context: Context): Vibrator? {
    val v = if (Build.VERSION.SDK_INT >= 31) {
        context.getSystemService(VibratorManager::class.java)?.defaultVibrator
    } else {
        @Suppress("DEPRECATION")
        context.getSystemService(Context.VIBRATOR_SERVICE) as? Vibrator
    }
    return v?.takeIf { it.hasVibrator() }
}

/**
 * [body] when it plays [dev]'s rumble, else null. A controller built into this device with no
 * motor of its own rumbles through the body: on a handheld the body is the controller. An
 * external pad without motors stays silent.
 */
fun bodyRumbleFor(dev: InputDevice, body: Vibrator?): Vibrator? =
    body?.takeIf { rumblesOnBody(padHasMotor(dev), dev.isExternalDevice()) }

/** [bodyRumbleFor]'s rule on plain facts. */
internal fun rumblesOnBody(padHasMotor: Boolean, external: Boolean): Boolean =
    !padHasMotor && !external

/** Whether [dev] reports a rumble motor — via VibratorManager (API 31+) or the legacy Vibrator. */
fun padHasMotor(dev: InputDevice): Boolean =
    if (Build.VERSION.SDK_INT >= 31) {
        dev.vibratorManager.vibratorIds.isNotEmpty()
    } else {
        @Suppress("DEPRECATION")
        dev.vibrator.hasVibrator()
    }

/** Below API 29 there is no `isExternal`; built-in keys and the nav bar carry no vendor id. */
fun InputDevice.isExternalDevice(): Boolean =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) isExternal else vendorId != 0
