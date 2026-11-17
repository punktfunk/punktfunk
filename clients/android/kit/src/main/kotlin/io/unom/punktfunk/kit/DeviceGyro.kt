package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.view.Display
import android.view.Surface
import android.view.WindowManager

/**
 * The opt-in phone-gyro mirror ("Gyro from this phone", off by default): while wire pad 0 is a
 * controller with no motion source of its own, THIS device's IMU speaks for it on the rich-input
 * motion plane — for clip-on and third-party pads that ship without a gyro, where the phone body
 * is rigidly attached to (or simply is) the thing in the player's hands. [GamepadFeedback]'s
 * rumble-on-phone mirror with the data flowing the other way.
 *
 * A pad's own motion comes from a capture link (USB DualSense / SC2, claimed as
 * [GamepadRouter.ExternalPad]s) or, on API 31+, from [PadSensors] reading a Bluetooth pad's IMU,
 * so the stand-down rule is exactly [GamepadRouter.padHasOwnMotion]: when either holds pad 0,
 * the mirror sends nothing — two motion writers on one wire pad would fight. It also sends
 * nothing while pad 0 has no slot at all (motion never creates a host pad; a controller must
 * have arrived first).
 *
 * Two properties this class enforces itself:
 *  - samples ride a dedicated [HandlerThread] with batching disabled (`maxReportLatencyUs = 0`) —
 *    sensor batching would trade the exact latency gyro aim exists to avoid;
 *  - a stand-down edge (a pad with its own motion takes pad 0, or [stop]) sends ONE zero-gyro
 *    sample, so the host's virtual pad never keeps re-emitting an angular velocity this device
 *    stopped producing.
 *
 * Units are the wire contract, converted by [Gamepad.motionGyroWire] / [Gamepad.motionAccelWire] —
 * the same two functions [PadSensors] uses, so a scale this client ever has to correct is corrected
 * once for every sender rather than once per sender that someone remembers. The one thing the
 * phone adds is a frame remap: sensors report in the device's natural-portrait frame, while
 * the wire wants the controller frame the player sees (x right, y up, z out of the screen), so
 * each sample is rotated by the current display rotation — a phone clipped landscape must yaw
 * when the player yaws, not roll. The matrix is derived and pinned by `DeviceGyroVectorTest`
 * (the table Apple's `DeviceGyroRemap` runs too); correctable in one place if on-glass says
 * otherwise.
 */
class DeviceGyro(
    context: Context,
    private val handle: Long,
    private val router: GamepadRouter,
) : SensorEventListener {

    private val sensorManager: SensorManager? =
        context.getSystemService(SensorManager::class.java)

    /** For the live rotation; null on contexts without a display association (then portrait). */
    private val display: Display? = runCatching {
        if (Build.VERSION.SDK_INT >= 30) {
            context.display
        } else {
            @Suppress("DEPRECATION")
            context.getSystemService(WindowManager::class.java)?.defaultDisplay
        }
    }.getOrNull()

    private val thread = HandlerThread("pf-phone-gyro")

    /** Latest converted accel, paired with each gyro send (the wire fuses both per sample). */
    private val lastAccel = intArrayOf(0, Gamepad.MOTION_ACCEL_LSB_PER_G, 0)

    /** Whether the last gyro event actually went to pad 0 — the stand-down zero-send edge. */
    private var wasWriting = false

    /** Register the listeners; a device without a gyroscope makes this a no-op. */
    fun start() {
        val sm = sensorManager ?: return
        val gyro = sm.getDefaultSensor(Sensor.TYPE_GYROSCOPE) ?: return
        thread.start()
        val h = Handler(thread.looper)
        // ~200 Hz requested (the framework clamps to what the hardware offers), zero report
        // latency: batching is poison for gyro aim.
        sm.registerListener(this, gyro, SAMPLING_PERIOD_US, 0, h)
        sm.getDefaultSensor(Sensor.TYPE_ACCELEROMETER)?.let {
            sm.registerListener(this, it, SAMPLING_PERIOD_US, 0, h)
        }
    }

    /**
     * Unregister and join the sensor thread, then park the host pad's rotation at zero if this
     * mirror was the live writer. Call BEFORE the router is released / the handle freed —
     * teardown-ordered like the feedback threads.
     */
    fun stop() {
        sensorManager?.unregisterListener(this)
        thread.quitSafely()
        runCatching { thread.join() }
        if (wasWriting) {
            wasWriting = false
            sendZero()
        }
    }

    override fun onSensorChanged(event: SensorEvent) {
        val rotation = display?.rotation ?: Surface.ROTATION_0
        when (event.sensor.type) {
            Sensor.TYPE_ACCELEROMETER -> {
                val v = remap(rotation, event.values[0], event.values[1], event.values[2])
                for (i in 0..2) lastAccel[i] = Gamepad.motionAccelWire(v[i])
            }
            Sensor.TYPE_GYROSCOPE -> {
                // The write gate, per sample: sends must be on at all (the forwarding
                // preference AND the session's GAMEPAD grant — an AccessUpdate can revoke it
                // mid-session), pad 0 must exist (motion never creates a pad) and must not have
                // a motion source of its own (its IMU is streaming).
                val write = router.sendsEnabled() && router.padPresent(0) &&
                    !router.padHasOwnMotion(0)
                if (!write) {
                    // Stand-down edge: never leave the last angular velocity latched host-side.
                    if (wasWriting) {
                        wasWriting = false
                        sendZero()
                    }
                    return
                }
                wasWriting = true
                val v = remap(rotation, event.values[0], event.values[1], event.values[2])
                NativeBridge.nativeSendPadMotion(
                    handle, 0,
                    Gamepad.motionGyroWire(v[0]),
                    Gamepad.motionGyroWire(v[1]),
                    Gamepad.motionGyroWire(v[2]),
                    lastAccel[0], lastAccel[1], lastAccel[2],
                )
            }
        }
    }

    override fun onAccuracyChanged(sensor: Sensor?, accuracy: Int) {}

    /** Zero rotation, last-known accel — "at rest", not free-fall. */
    private fun sendZero() {
        NativeBridge.nativeSendPadMotion(
            handle, 0, 0, 0, 0, lastAccel[0], lastAccel[1], lastAccel[2],
        )
    }

    companion object {
        /** Whether this device can source motion at all — gates the settings rows (a TV box
         *  without an IMU would make the toggle a silent no-op, the rumble mirror's rule). */
        fun available(context: Context): Boolean =
            context.getSystemService(SensorManager::class.java)
                ?.getDefaultSensor(Sensor.TYPE_GYROSCOPE) != null

        /**
         * ~200 Hz — between the sensor's usual FASTEST (~250-500 Hz) and GAME (~50 Hz), and also
         * the ceiling the framework grants an app without `HIGH_SAMPLING_RATE_SENSORS` (API 31+),
         * so asking for more would only be silently capped. Shared with [PadSensors].
         */
        internal const val SAMPLING_PERIOD_US = 5000

        /**
         * Rotate one device-frame vector (rotation rate or acceleration — both transform the
         * same way under an in-plane rotation) into the controller frame for [rotation]
         * ([Surface].ROTATION_*). Sensors report in the natural-portrait frame (+x right edge,
         * +y top, +z out of the screen); the controller frame keeps +z (the screen always faces
         * the player) and rotates x/y to mean "player's right" and "player's up". ROTATION_90 =
         * the device physically turned counter-clockwise, top to the player's LEFT.
         */
        fun remap(rotation: Int, x: Float, y: Float, z: Float): FloatArray = when (rotation) {
            Surface.ROTATION_90 -> floatArrayOf(-y, x, z) // top left: right = bottom, up = +x
            Surface.ROTATION_270 -> floatArrayOf(y, -x, z) // top right: right = top, up = −x
            Surface.ROTATION_180 -> floatArrayOf(-x, -y, z)
            else -> floatArrayOf(x, y, z)
        }
    }
}
