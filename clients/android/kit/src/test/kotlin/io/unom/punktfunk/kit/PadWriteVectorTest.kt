package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * [hostWriteRefused] against `clients/shared/pad-write-vectors.json`, which the desktop core's
 * `refused` and Swift's `refusedWrite` replay too.
 */
class PadWriteVectorTest {
    @Test
    fun hostWritesMatchTheSharedVectors() {
        // Gradle runs unit tests with the module dir as cwd (clients/android/kit).
        val file = File("../../shared/pad-write-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        val root = JSONObject(file.readText())
        fun bytes(a: JSONArray) = (0 until a.length()).map { a.getInt(it) }
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
