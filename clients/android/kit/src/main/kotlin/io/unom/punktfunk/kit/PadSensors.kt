package io.unom.punktfunk.kit

import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.util.Log
import android.view.InputDevice
import java.util.concurrent.ConcurrentHashMap

/**
 * Motion from a controller the Android input stack owns — the Bluetooth pads.
 *
 * A DualSense, a DualShock 4, a Switch Pro or an 8BitDo paired over BLUETOOTH is not a capture
 * link ([DsCapture] claims a Sony pad over USB, [Sc2Capture] a Steam Controller 2): it arrives as
 * an ordinary [InputDevice], and its motion is reachable only through the platform sensor API.
 *
 * Android 12 (API 31) exposes those sensors: [InputDevice.getSensorManager] hands back a
 * [android.hardware.SensorManager] scoped to that one controller, carrying the usual
 * TYPE_GYROSCOPE / TYPE_ACCELEROMETER. This class registers a listener per forwarded controller
 * that has a gyroscope, converts each sample to wire units, and sends it on that pad's wire index
 * through [GamepadRouter.deviceMotion]. Below API 31 nothing is registered and the class is inert:
 * those pads keep working, minus motion.
 *
 * It follows [DeviceGyro] (the phone-gyro mirror) wherever the two solve the same problem:
 *  - samples ride ONE dedicated [HandlerThread] with batching disabled (`maxReportLatencyUs = 0`)
 *    — sensor batching would trade away the exact latency gyro aim exists to avoid, and the main
 *    thread is where Compose recomposition lives;
 *  - a feed torn down while its wire pad is still alive parks the rotation at zero first, because
 *    the host holds motion as STATE and re-emits it in every virtual-pad report: an angular
 *    velocity left behind reads as a pad rotating forever.
 *
 * One writer per pad, three ways:
 *  1. A USB capture claims the physical device away from the input stack; [DsCapture.startUsb]
 *     calls [GamepadRouter.releaseDevice] at claim time, which closes the slot, which fires
 *     `onSlotClosed`, which lands on [onSlotClosed] here and unregisters. The claim also makes the
 *     controller's [InputDevice] vanish outright, so even a reopened slot would find nothing to
 *     register — but the explicit teardown is what makes the ordering deterministic instead of a
 *     race against the platform's own removal callback.
 *  2. The phone-gyro mirror stands down: registering flips
 *     [GamepadRouter.setDeviceHasSensorMotion], [GamepadRouter.padHasOwnMotion] reports it, and
 *     [DeviceGyro] re-reads that gate on every sample (sending its own zero park on the edge).
 *  3. Exactly one feed exists per deviceId — [onSlotOpened] is idempotent, and it is the only
 *     thing that ever constructs one.
 *
 * Frame: see [gyroToWire] — the mapping is straight through.
 */
class PadSensors(private val router: GamepadRouter) {

    /** One controller's live sensor feed: its listener state and the accel it pairs with each
     *  rotation. Its arrays belong to the sensor thread; [stop] reads them only after the join. */
    private inner class Feed(
        private val deviceId: Int,
        /** The manager it registered with: a removed device can no longer hand it back. */
        val sensors: SensorManager,
    ) : SensorEventListener {
        /** Latest converted accel, paired with each gyro send (the wire fuses both per sample).
         *  Starts at the host's neutral — 1 g on the up axis, NOT [0,0,0], which is free fall. */
        private val accel = intArrayOf(0, Gamepad.MOTION_ACCEL_LSB_PER_G, 0)
        private val gyro = IntArray(3)

        /** Whether any rotation has gone out on this pad — gates the park on teardown, so a pad
         *  that never sent motion is not handed a sample it did not earn. */
        @Volatile
        var wroteMotion = false
            private set

        override fun onSensorChanged(event: SensorEvent) {
            when (event.sensor.type) {
                Sensor.TYPE_ACCELEROMETER -> accelToWire(event.values, accel)
                Sensor.TYPE_GYROSCOPE -> {
                    gyroToWire(event.values, gyro)
                    // One line per controller per session, on its first gyro sample. With the pad
                    // flat and still, the accel triple shows which slot gravity lands on — the
                    // check that the straight-through mapping in [gyroToWire] holds.
                    if (!wroteMotion) {
                        Log.i(
                            TAG,
                            "controller $deviceId first motion sample: " +
                                "gyro ${gyro.joinToString()} accel ${accel.joinToString()}",
                        )
                    }
                    wroteMotion = true
                    router.deviceMotion(deviceId, gyro, accel)
                }
            }
        }

        override fun onAccuracyChanged(sensor: Sensor?, accuracy: Int) {}

        /** Zero rotation, last-known accel — "at rest", not free fall. */
        fun park() {
            gyro.fill(0)
            router.deviceMotion(deviceId, gyro, accel)
        }
    }

    /** deviceId → live feed. Concurrent: the main thread mutates it while the sensor thread is
     *  running (hot-plug, a capture link's claim). */
    private val feeds = ConcurrentHashMap<Int, Feed>()

    private val thread = HandlerThread("pf-pad-sensors")
    private var handler: Handler? = null

    /**
     * Start the sensor thread and attach to every controller the router already forwards — the
     * pads connected before the session opened, which will never fire a hot-plug callback.
     * Everything after that arrives through [onSlotOpened]. Main thread.
     */
    fun start() {
        if (!supported()) return
        thread.start()
        handler = Handler(thread.looper)
        for (deviceId in router.forwardedDevices()) onSlotOpened(deviceId)
    }

    /**
     * A slot opened for real controller [deviceId] — attach if it has a gyroscope of its own.
     * Idempotent, and a no-op before [start] or on a platform without the API. Main thread, from
     * [GamepadRouter.onSlotOpened].
     */
    fun onSlotOpened(deviceId: Int) {
        val h = handler ?: return
        if (feeds.containsKey(deviceId)) return
        // API 31+ only — getSensorManager does not exist below it. Re-checked here rather than
        // relying on start()'s gate, so the entry point is safe on its own terms.
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return
        val dev = InputDevice.getDevice(deviceId) ?: return
        // Declared non-null: a controller with no sensors gets an empty manager, not a null one.
        val sm = dev.sensorManager
        // A gyroscope is the entry price: an accel-only feed would send gravity and pin rotation
        // at zero on a pad the phone-gyro mirror is otherwise entitled to speak for (two writers).
        // No gyroscope here is not proof the pad has no IMU: a pad's motion can sit on its own
        // evdev node, which isForwardable filters out, if the platform does not merge the two.
        val gyroSensor = sm.getDefaultSensor(Sensor.TYPE_GYROSCOPE) ?: return
        val feed = Feed(deviceId, sm)
        feeds[deviceId] = feed
        // ~200 Hz requested, zero report latency: batching is poison for gyro aim, and 200 Hz is
        // what the framework grants an app without HIGH_SAMPLING_RATE_SENSORS anyway.
        sm.registerListener(feed, gyroSensor, DeviceGyro.SAMPLING_PERIOD_US, 0, h)
        sm.getDefaultSensor(Sensor.TYPE_ACCELEROMETER)?.let {
            sm.registerListener(feed, it, DeviceGyro.SAMPLING_PERIOD_US, 0, h)
        }
        // The pad sources its own rotation from here on → the phone-gyro mirror stands down for it.
        router.setDeviceHasSensorMotion(deviceId, true)
        Log.i(TAG, "controller $deviceId (${dev.name}) has a gyro — forwarding its motion")
    }

    /**
     * The slot for [deviceId] closed — unplug, session teardown, or a capture link claiming the
     * device. Unregister and hand the pad back to the phone-gyro mirror. Main thread, from
     * [GamepadRouter.onSlotClosed].
     *
     * No park-at-zero here, on purpose: the router removed the slot BEFORE invoking the callback
     * and has already sent that pad's Remove, so the host tore the virtual pad down and there is no
     * latched rotation left to clear — while writing to a wire index that is free again would be
     * addressing whoever claims it next. [stop] is the case where the pad outlives the feed.
     */
    fun onSlotClosed(deviceId: Int) {
        unregister(deviceId)
        router.setDeviceHasSensorMotion(deviceId, false)
    }

    /**
     * Unregister every listener, join the sensor thread, then park at zero each pad that was
     * rotating. Call BEFORE the router is released and the session handle freed — the same
     * teardown ordering rule as the feedback poll threads and [DeviceGyro.stop]. The parks come
     * AFTER the join for two reasons: a sample still in flight would re-latch the rotation just
     * cleared, and the join is what publishes the sensor thread's writes to this one.
     */
    fun stop() {
        val parked = feeds.keys.toList().mapNotNull { id -> unregister(id)?.let { id to it } }
        for ((deviceId, _) in parked) router.setDeviceHasSensorMotion(deviceId, false)
        thread.quitSafely()
        runCatching { thread.join() }
        handler = null
        for ((_, feed) in parked) if (feed.wroteMotion) feed.park()
    }

    /**
     * Drop [deviceId]'s listeners, returning the feed that held them (null if there was none).
     * Through the manager the feed registered with: after a disconnect [InputDevice.getDevice]
     * is null, and the platform keeps the listener (and this session through it) otherwise.
     */
    private fun unregister(deviceId: Int): Feed? {
        val feed = feeds.remove(deviceId) ?: return null
        feed.sensors.unregisterListener(feed)
        return feed
    }

    companion object {
        private const val TAG = "PadSensors"

        /** Whether this platform can read a controller's own sensors at all (API 31+). */
        fun supported(): Boolean = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S

        /**
         * One gyroscope sample (Android: rad/s) → the wire's three signed-16 components, in place.
         *
         * The axis frame is straight through.
         *
         * The wire is a unit passthrough into a virtual DualSense report: slot 0 = Right (pitch),
         * slot 1 = Up (yaw), slot 2 = Backward toward the player (roll), right-handed. Android
         * hands a controller's own sensors over in that same frame. Measured with a DualSense on
         * Bluetooth: the first-sample log read `accel 0, 10000, 0` (1 g on slot 1) and each
         * rotation drove its correctly-named axis on the host.
         *
         * So: no remap. If a device disagrees, the remap belongs HERE with its own expectations
         * in `PadSensorsTest` — not spread across callers.
         */
        fun gyroToWire(values: FloatArray, out: IntArray) {
            for (i in 0..2) out[i] = Gamepad.motionGyroWire(values.getOrElse(i) { 0f })
        }

        /**
         * One accelerometer sample (Android: m/s², specific force) → the wire's three signed-16
         * components, in place. Same measured frame as [gyroToWire] and the same straight-through
         * mapping; the sign needs no flip, because Android and the DualSense report agree that the
         * axis pointing up reads +1 g at rest (see [Gamepad.motionAccelWire]): a pad lying flat
         * reads `accel 0, 10000, 0`.
         */
        fun accelToWire(values: FloatArray, out: IntArray) {
            for (i in 0..2) out[i] = Gamepad.motionAccelWire(values.getOrElse(i) { 0f })
        }
    }
}
