package io.unom.punktfunk

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Replays `clients/shared/key-row-vectors.json`, which the Apple client replays too: what a key
 * row button sends on a tap and a long press, and what the latch does with typed text. A new
 * rule belongs in that file. Run: `./gradlew :app:testDebugUnitTest`.
 */
class KeyRowTest {
    private val vectors: JSONObject by lazy {
        // The module directory is the working directory, so clients/ is two levels up.
        val file = File("../../shared/key-row-vectors.json")
        assertTrue("the vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private fun hex(vk: Int) = "%02X".format(vk)

    @Test
    fun sharedVectorsSendWhatTheySay() {
        val cases = vectors.getJSONArray("cases")
        assertTrue("the vector file is the contract", cases.length() >= 10)
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val name = case.getString("name")
            val out = mutableListOf<String>()
            val latch = KeyLatch { vk, down -> out += (if (down) "+" else "-") + hex(vk) }
            val buttons = HashMap<String, RowButton>()
            fun button(id: String) = buttons.getOrPut(id) {
                RowButton(latch, requireNotNull(RowSlot.parse(id)) { "$name: $id" }) { action, phase ->
                    out += "fire:${phase.name.lowercase()}:${action.id}"
                }
            }
            val steps = case.getJSONArray("steps")
            for (s in 0 until steps.length()) {
                val step = steps.getJSONArray(s)
                when (val verb = step.getString(0)) {
                    "tap" -> button(step.getString(1)).tap()
                    "hold" -> button(step.getString(1)).longPress()
                    "lift" -> button(step.getString(1)).end()
                    "type" -> latch.type(step.getString(1)) { out += "text:$it" }
                    "release" -> latch.release()
                    else -> error("$name: step $verb")
                }
            }
            val want = case.getJSONArray("out")
            assertEquals(name, (0 until want.length()).map(want::getString), out)
        }
    }

    @Test
    fun aTypedCharacterFindsItsKeyInTheUsOrder() {
        val typed = vectors.getJSONArray("typed")
        assertTrue(typed.length() > 0)
        for (i in 0 until typed.length()) {
            val row = typed.getJSONArray(i)
            val c = row.getString(0).single()
            assertEquals("typed ${row.getString(0)}", if (row.isNull(1)) null else row.getString(1), typedVk(c)?.let(::hex))
        }
    }
}
