package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** Replays `clients/shared/profile-picker-vectors.json`, the rule's contract with Rust and Swift. */
class ProfilesTest {
    private fun opt(o: JSONObject, k: String) = if (o.isNull(k)) null else o.getString(k)

    @Test
    fun followsTheSharedPickerVectors() {
        val file = File("../../shared/profile-picker-vectors.json")
        assertTrue("vectors reachable at ${file.absolutePath}", file.isFile)
        val cases = JSONObject(file.readText()).getJSONArray("cases")
        assertTrue(cases.length() > 0)
        for (i in 0 until cases.length()) {
            val c = cases.getJSONObject(i)
            val name = c.getString("name")
            val listed = c.optJSONArray("listed")?.let { a ->
                (0 until a.length()).map { ListedProfile.parse(a.getJSONObject(it)) }
            }
            val remembered = c.optJSONObject("remembered")?.let {
                ProfilePick(it.getString("id"), it.optString("display_name"))
            }
            val d = pickerDecision(listed, remembered, opt(c, "link"))
            val want = c.getJSONObject("expect")
            assertEquals("$name: picker", want.getBoolean("picker"), d.picker)
            assertEquals("$name: send", opt(want, "send"), d.send)
            assertEquals("$name: gone", opt(want, "gone"), d.gone)
            assertEquals("$name: remember", opt(want, "remember"), d.remember?.id)
        }
    }

    @Test
    fun decodesAHostRowAndWordsItsSeat() {
        val rows = ListedProfile.parseList(
            """[{"id":"9a","display_name":"Kid","accent":"#f97316","future":1,
            "seat":{"state":"occupied","port":9777,"occupant":"Ben's Apple TV"}},
            {"id":"x","display_name":"Odd","seat":{"state":"sleeping","port":1}}]""",
        )!!
        assertEquals("In use by Ben's Apple TV", rows[0].note())
        assertEquals(SeatState.OTHER, rows[1].seat!!.state)
        assertEquals("AL", initials("anna lena x"))
    }
}
