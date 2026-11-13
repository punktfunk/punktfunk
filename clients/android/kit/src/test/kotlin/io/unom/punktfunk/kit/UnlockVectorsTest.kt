package io.unom.punktfunk.kit

import java.io.File
import java.security.MessageDigest
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Replays every section of `clients/shared/profile-unlock-vectors.json`, the unlock's contract with Rust and Swift. */
class UnlockVectorsTest {
    private val vectors = File("../../shared/profile-unlock-vectors.json").let {
        assertTrue("vectors reachable at ${it.absolutePath}", it.isFile)
        JSONObject(it.readText())
    }

    private fun cases(section: String): List<JSONObject> {
        val a = vectors.getJSONArray(section)
        assertTrue(section, a.length() > 0)
        return (0 until a.length()).map { a.getJSONObject(it) }
    }

    private fun opt(o: JSONObject, k: String) = if (o.isNull(k)) null else o.getString(k)
    private fun need(o: JSONObject) = UnlockNeed.valueOf(o.getString("need").uppercase())
    private fun gate(o: JSONObject) = UnlockGate.valueOf(o.getString("gate").uppercase())
    private fun hex(b: ByteArray) = b.joinToString("") { "%02x".format(it) }
    private fun bytes(hex: String) = ByteArray(hex.length / 2) { hex.substring(it * 2, it * 2 + 2).toInt(16).toByte() }

    @Test
    fun eachWordReadsAsItsNeedAndItsForm() {
        for (c in cases("unlock_words")) {
            val row = JSONObject().put("id", "kid")
            opt(c, "word")?.let { row.put("unlock", it) }
            val read = ListedProfile.parse(row).unlock
            assertEquals("$c", need(c), read)
            val form = if (read == UnlockNeed.NONE) null else if (read.pin) "pin" else "password"
            assertEquals("$c", opt(c, "form"), form)
        }
    }

    @Test
    fun theGateBeforeADialFollowsTheVectors() {
        for (c in cases("gate")) {
            val holds = c.getBoolean("holds_key")
            assertEquals("$c", gate(c), beforeDial(need(c), holds))
            assertEquals("$c", c.getBoolean("delete_key"), staleKey(need(c), holds))
        }
    }

    @Test
    fun aRefusalLeadsWhereTheVectorsSay() {
        for (c in cases("after_refusal")) {
            assertEquals("$c", gate(c), afterRefusal(need(c), c.getBoolean("holds_key"), c.getBoolean("sent")))
        }
    }

    @Test
    fun aPinIsItsAsciiDigitsOrTheLineThatSaysWhy() {
        for (c in cases("pins")) {
            val typed = c.getString("typed").toCharArray()
            val sent = opt(c, "sent")
            if (sent != null) {
                assertArrayEquals("$c", sent.toByteArray(Charsets.US_ASCII), pinDigits(typed))
            } else {
                assertNull("$c", pinDigits(typed))
                val line = if (c.getString("error") == "empty") R.string.unlock_pin_error_empty else R.string.unlock_pin_error_shape
                assertEquals("$c", say(line), pinError(typed.isEmpty()))
            }
        }
    }

    @Test
    fun aChallengeAnswerNamesASeatAProfileOrNothing() {
        for (c in cases("challenge_answers")) {
            val parsed = UnlockChallenge.parse(c.getJSONObject("body").toString())
            if (opt(c, "seat") == null && opt(c, "profile") == null) {
                assertNull("$c", parsed)
                continue
            }
            assertEquals("$c", opt(c, "seat"), parsed!!.seat)
            assertEquals("$c", opt(c, "profile"), parsed.profile)
            assertArrayEquals(ByteArray(32) { 0x0a }, parsed.challenge)
        }
    }

    @Test
    fun anIdTheMessageRefusesIsNoChallenge() {
        for (c in cases("refused")) {
            val id = c.getString("id")
            val body = JSONObject().put("challenge", "0a".repeat(32)).put(c.getString("form"), id)
            assertNull("$c", UnlockChallenge.parse(body.toString()))
            if (c.getString("form") == "profile") assertFalse("$c", isProfileId(id))
        }
    }

    /**
     * The native builder signs what the parsed challenge names: each message is that challenge,
     * the seat id or the profile id zero-padded to 64, then the device's and the box's fingerprints.
     */
    @Test
    fun aChallengeHandsTheBuilderWhatEachMessageNames() {
        for (c in cases("messages")) {
            val form = c.getString("form")
            val id = c.getString(form)
            val body = JSONObject().put("challenge", c.getString("challenge")).put(form, id)
            val parsed = UnlockChallenge.parse(body.toString())!!
            val (label, named) = when (form) {
                "seat" -> "punktfunk-seat-unlock-v1" to parsed.seat!!.toByteArray()
                else -> "punktfunk-profile-unlock-v1" to parsed.profile!!.toByteArray().copyOf(64)
            }
            val m = label.toByteArray() + parsed.challenge + named + bytes(c.getString("device")) + bytes(c.getString("box"))
            assertEquals(c.getString("name"), if (form == "seat") 152 else 187, m.size)
            opt(c, "message")?.let { assertEquals(c.getString("name"), it, hex(m)) }
            assertEquals(c.getString("name"), c.getString("sha256"), hex(MessageDigest.getInstance("SHA-256").digest(m)))
        }
    }
}
