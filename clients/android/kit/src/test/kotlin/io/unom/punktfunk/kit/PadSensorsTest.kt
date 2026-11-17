package io.unom.punktfunk.kit

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Pins the unit scaling and the axis mapping of the controller-sensor path ([PadSensors]) and the
 * shared converters it goes through ([Gamepad.motionGyroWire] / [Gamepad.motionAccelWire]). Pure
 * JVM — the two `*ToWire` functions take plain float arrays and touch no Android class.
 *
 * The scale is the wire contract (`punktfunk_core::input::gamepad`: 20 LSB/°·s, 10000 LSB/g) and
 * must not drift. The axis mapping is straight through; [straightThroughFrame] makes a remap a
 * deliberate, visible edit rather than a quiet one.
 * Run: `./gradlew :kit:testDebugUnitTest`.
 */
class PadSensorsTest {
    private fun gyro(x: Float, y: Float, z: Float) =
        IntArray(3).also { PadSensors.gyroToWire(floatArrayOf(x, y, z), it) }

    private fun accel(x: Float, y: Float, z: Float) =
        IntArray(3).also { PadSensors.accelToWire(floatArrayOf(x, y, z), it) }

    /** 20 LSB/°·s from Android's rad/s: π rad/s is exactly 180 °/s, so exactly 3600 raw. */
    @Test
    fun gyroScaleFromRadiansPerSecond() {
        assertEquals(3600, gyro(Math.PI.toFloat(), 0f, 0f)[0])
        assertEquals(-3600, gyro(-Math.PI.toFloat(), 0f, 0f)[0])
        assertEquals(1146, gyro(1f, 0f, 0f)[0]) // 1 rad/s ⇒ 1145.9156, rounded
        assertEquals(0, gyro(0f, 0f, 0f)[0])
    }

    /** 10000 LSB/g from Android's m/s²: standard gravity is exactly 1 g. Android reports specific
     *  force, so a pad at rest reads +1 g on the axis pointing up — no sign flip anywhere. */
    @Test
    fun accelScaleFromMetresPerSecondSquared() {
        assertEquals(10_000, accel(0f, Gamepad.GRAVITY, 0f)[1])
        assertEquals(-10_000, accel(0f, -Gamepad.GRAVITY, 0f)[1])
        assertEquals(0, accel(0f, 0f, 0f)[1])
    }

    /** A controller lying flat and still lands exactly on the host's neutral for a virtual
     *  DualSense — 1 g on wire slot 1 (`punktfunk-core` `MOTION_NEUTRAL_ACCEL = [0, 10000, 0]`),
     *  not the [0,0,0] that means free fall. */
    @Test
    fun restingPadIsTheHostNeutral() {
        assertArrayEquals(intArrayOf(0, 10_000, 0), accel(0f, Gamepad.GRAVITY, 0f))
    }

    /**
     * The frame: component i of the sensor sample becomes component i of the wire triple, for both
     * planes, with no permutation and no negation. A device that needs a remap gets it in
     * [PadSensors.gyroToWire], and this test changes with it. Distinct magnitudes per axis so a
     * swap or a flip cannot cancel out.
     */
    @Test
    fun straightThroughFrame() {
        assertArrayEquals(intArrayOf(1146, 2292, 3438), gyro(1f, 2f, 3f))
        assertArrayEquals(
            intArrayOf(10_000, 20_000, -30_000),
            accel(Gamepad.GRAVITY, 2f * Gamepad.GRAVITY, -3f * Gamepad.GRAVITY),
        )
    }

    /** Both planes clamp to signed 16 bits rather than wrapping — a flick past 1638 °/s or a knock
     *  past 3.27 g saturates, where a wrap would send a full-speed rotation the other way. */
    @Test
    fun clampsToSigned16() {
        assertArrayEquals(intArrayOf(32767, -32768, 32767), gyro(100f, -100f, 1e9f))
        assertArrayEquals(intArrayOf(32767, -32768, 32767), accel(1000f, -1000f, 1e9f))
    }

    /** Rounds to nearest rather than truncating: a truncating converter loses up to a whole LSB
     *  off every sample, always toward zero, and a gyro whose every sample is biased the same way
     *  is a gyro that drifts. */
    @Test
    fun roundsToNearestNotTowardZero() {
        assertEquals(1, gyro(0.0006f, 0f, 0f)[0]) // 0.688 raw — truncation would say 0
        assertEquals(-1, gyro(-0.0006f, 0f, 0f)[0])
        assertEquals(1, accel(0.0007f, 0f, 0f)[0]) // 0.714 raw
    }

    /** A sensor that hands back fewer than three components (or none — the framework reuses one
     *  array across types) contributes zero rather than throwing on the sensor thread. */
    @Test
    fun shortSampleIsZeroFilled() {
        val out = IntArray(3) { 7 }
        PadSensors.gyroToWire(floatArrayOf(Math.PI.toFloat()), out)
        assertArrayEquals(intArrayOf(3600, 0, 0), out)
    }
}
