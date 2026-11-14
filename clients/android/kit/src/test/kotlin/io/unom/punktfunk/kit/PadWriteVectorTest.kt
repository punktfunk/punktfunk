package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * [hostWriteRefused] and the Bluetooth framing against `clients/shared/pad-write-vectors.json`,
 * which the desktop core's `refused` and Swift's `refusedWrite` and `Sc2Device` replay too.
 */
class PadWriteVectorTest {
    private fun bytes(a: JSONArray) = (0 until a.length()).map { a.getInt(it) }

    private fun shared(): JSONObject {
        // Gradle runs unit tests with the module dir as cwd (clients/android/kit).
        val file = File("../../shared/pad-write-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        return JSONObject(file.readText())
    }

    @Test
    fun bluetoothWritesMatchTheSharedVectors() {
        val root = shared()
        val heldBack = bytes(root.getJSONArray("triton_held_back"))
        for (cmd in 0..0xFF) {
            val feature = Sc2Device.featurePayload(byteArrayOf(1, cmd.toByte(), 0))
            assertEquals("feature 0x%02x".format(cmd), cmd in heldBack, feature == null)
            val output = Sc2Device.outputWrite(byteArrayOf(cmd.toByte(), 0))
            assertEquals("output 0x%02x".format(cmd), cmd in 0x80..0x89, output != null)
            assertTrue(output?.charUuid != Sc2Device.BLE_FEATURE_CHAR)
        }
        val rows = root.getJSONArray("triton_ble")
        for (i in 0 until rows.length()) {
            val row = rows.getJSONObject(i)
            val data = bytes(row.getJSONArray("data")).map { it.toByte() }.toByteArray()
            val got = if (row.getInt("kind") == 0) Sc2Device.outputWrite(data) else Sc2Device.featurePayload(data)
            assertEquals(row.toString(), row.getBoolean("refused"), got == null)
        }
    }

    @Test
    fun hostWritesMatchTheSharedVectors() {
        val root = shared()
        val forwarded = bytes(root.getJSONArray("classic_forwarded"))
        val heldBack = bytes(root.getJSONArray("triton_held_back"))
        for (cmd in 0..0xFF) {
            val classic = hostWriteRefused(true, 1, byteArrayOf(0, cmd.toByte(), 0))
            assertEquals("2015 pad 0x%02x".format(cmd), cmd !in forwarded, classic != null)
            val triton = hostWriteRefused(false, 1, byteArrayOf(1, cmd.toByte(), 0))
            assertEquals("triton 0x%02x".format(cmd), cmd in heldBack, triton != null)
        }
        val frames = root.getJSONArray("frames")
        for (i in 0 until frames.length()) {
            val row = frames.getJSONObject(i)
            val model = row.getString("model")
            assertTrue(model, model == "classic" || model == "triton")
            val data = bytes(row.getJSONArray("data")).map { it.toByte() }.toByteArray()
            val got = hostWriteRefused(model == "classic", row.getInt("kind"), data)
            assertEquals(row.toString(), row.getBoolean("refused"), got != null)
        }
    }
}
