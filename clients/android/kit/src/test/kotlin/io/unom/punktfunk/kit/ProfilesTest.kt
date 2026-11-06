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
        assertEquals(say(R.string.profile_in_use_by, "Ben's Apple TV"), rows[0].note())
        assertEquals(SeatState.OTHER, rows[1].seat!!.state)
        assertEquals("AL", initials("anna lena x"))
    }

    @Test
    fun aKeptPickFollowsItsProfilesNewPictureAndSurvivesTheStore() {
        val old = ProfilePick("kid", "Kid", remember = true, avatar = "/kid?v=old")
        val rows = listOf(
            ListedProfile(id = "kid", displayName = "Kid", avatar = "/kid?v=new"),
            ListedProfile(id = "own", displayName = "Ada"),
        )
        val kept = pickerDecision(rows, old, null).remember!!
        assertEquals("/kid?v=new", kept.avatar)
        assertEquals(kept, ProfilePick.fromJson(kept.toJson()))
        assertEquals(ProfilePick("kid", "Kid"), ProfilePick.fromJson(ProfilePick("kid", "Kid").toJson()))
    }

    @Test
    fun aPictureMustBeAPathOnTheHost() {
        val base = "https://10.0.0.2:47990"
        assertEquals("$base/kid?v=1", pictureUrl(base, "/kid?v=1"))
        assertEquals(null, pictureUrl(base, "https://elsewhere.example/x.png"))
        assertEquals(null, pictureUrl(base, null))
    }

    @Test
    fun aSeatIsDialedWokenWaitedForOrRefused() {
        fun row(state: String, detail: String? = null) = ListedProfile.parse(
            JSONObject().put("id", "kid").put("display_name", "Kid").put(
                "seat",
                JSONObject().put("state", state).put("detail", detail ?: JSONObject.NULL).put("port", 9778),
            ),
        )
        assertEquals(SeatGate.Dial, seatGate(ListedProfile.parse(JSONObject().put("id", "own"))))
        assertEquals(SeatGate.Dial, seatGate(row("ready")))
        assertEquals(SeatGate.Dial, seatGate(row("occupied")))
        assertEquals(SeatGate.Dial, seatGate(row("sleeping")))
        assertEquals(SeatGate.Wake, seatGate(row("stopped")))
        assertEquals(SeatGate.Wait(Say.Text("Signing in")), seatGate(row("starting", "Signing in")))
        assertEquals(SeatGate.Wait(null), seatGate(row("starting")))
        assertEquals(SeatGate.Refuse(Say.Text("Seats are off.")), seatGate(row("unavailable", "Seats are off.")))
        assertEquals(SeatGate.Refuse(say(R.string.reject_seat_unavailable)), seatGate(row("unavailable")))
        assertEquals(say(R.string.profile_waking, "Kid"), wakingLine("Kid"))
    }

    @Test
    fun aLockedRowSaysWhatItNeedsAndDialsWhenStopped() {
        fun row(unlock: Any, state: String = "stopped") = ListedProfile.parse(
            JSONObject().put("id", "ada").put("unlock", unlock).put("seat", JSONObject().put("state", state)),
        )
        assertEquals(UnlockNeed.PASSWORD, row("password").unlock)
        assertEquals(UnlockNeed.KEY, row("key").unlock)
        assertEquals(UnlockNeed.NONE, row("none").unlock)
        assertEquals(UnlockNeed.NONE, row("retina").unlock)
        assertEquals(UnlockNeed.NONE, row(JSONObject.NULL).unlock)
        assertEquals(UnlockNeed.NONE, ListedProfile.parse(JSONObject().put("id", "old")).unlock)
        // Never a wake for a locked seat: the host refuses it, and the unlock on the dial starts it.
        assertEquals(SeatGate.Dial, seatGate(row("password")))
        assertEquals(SeatGate.Dial, seatGate(row("key")))
        assertEquals(SeatGate.Wake, seatGate(row("none")))
        assertEquals(SeatGate.Wait(null), seatGate(row("password", "starting")))
    }
}
