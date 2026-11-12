package io.unom.punktfunk

import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.UnlockChallenge
import io.unom.punktfunk.kit.UnlockNeed
import io.unom.punktfunk.kit.say
import java.security.KeyPair
import java.security.KeyPairGenerator
import java.security.MessageDigest
import java.security.Signature
import java.security.interfaces.ECPublicKey
import java.security.spec.ECGenParameterSpec
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** The touch shell's profile unlock (CONTRACT §6) over a software P-256 key and a scripted host. */
class ProfileUnlockTest {
    // The pinned box fingerprint, as the contract's vector has it: 32 × bb.
    private val hostFp = "bb".repeat(32)
    private val deviceFp = ByteArray(32) { 0xaa.toByte() }
    private val boxFp = ByteArray(32) { 0xbb.toByte() }
    private val vectorChallenge = ByteArray(32) { (it + 1).toByte() }
    private val vectorSeat = "0123456789abcdef0123456789abcdef"

    private fun row(unlock: UnlockNeed) = ListedProfile(id = "ada", displayName = "Ada", unlock = unlock)

    /** CONTRACT §3, built here byte by byte: the production builder is the native one. */
    private fun contractMessage(challenge: ByteArray, seat: String, device: ByteArray, box: ByteArray) =
        "punktfunk-seat-unlock-v1".toByteArray() + challenge + seat.toByteArray() + device + box

    @Test
    fun the_contract_message_matches_its_vector() {
        val m = contractMessage(vectorChallenge, vectorSeat, deviceFp, boxFp)
        assertEquals(152, m.size)
        assertEquals(
            "31de46821f3058729b14c3585ad13159085ecb0d18c87194a34b1bddc8ddc31a",
            MessageDigest.getInstance("SHA-256").digest(m).joinToString("") { "%02x".format(it) },
        )
    }

    @Test
    fun a_public_key_becomes_its_65_byte_point() {
        val pair = p256()
        val w = (pair.public as ECPublicKey).w
        fun fixed(n: java.math.BigInteger) = n.toByteArray().takeLast(32).toByteArray().let { ByteArray(32 - it.size) + it }
        val point = p256Point(pair.public.encoded)!!
        assertEquals(65, point.size)
        assertArrayEquals(byteArrayOf(4) + fixed(w.affineX) + fixed(w.affineY), point)
        // Any other curve or a cut encoding is no P-256 point.
        val p384 = KeyPairGenerator.getInstance("EC").apply { initialize(ECGenParameterSpec("secp384r1")) }.generateKeyPair()
        assertNull(p256Point(p384.public.encoded))
        assertNull(p256Point(pair.public.encoded.copyOf(90)))
    }

    @Test
    fun a_key_proof_verifies_over_the_contract_message() = runBlocking<Unit> {
        val keys = SoftKeys(biometric = true).apply { create(hostFp, "ada") }
        val host = Host()
        val sent = mutableListOf<Unlock?>()
        val hooks = hooks(keys, host, ask = { error("no prompt for a key unlock") })
        val dialed = dialUnlocked(hooks, hostFp, row(UnlockNeed.KEY)) { u ->
            sent += u?.let { Unlock(it.kind, it.secret.copyOf()) }
            HostServices.Cancelled
        }
        assertEquals(HostServices.Cancelled, dialed)
        val proof = sent.single()!!
        assertEquals(UNLOCK_KEY_PROOF, proof.kind)
        assertEquals(listOf("ada"), host.challenged)
        val message = contractMessage(vectorChallenge, vectorSeat, deviceFp, boxFp)
        val verify = Signature.getInstance("SHA256withECDSA").apply { initVerify(keys.pair!!.public) }
        verify.update(message)
        assertTrue(verify.verify(proof.secret))
    }

    @Test
    fun a_cancelled_prompt_dials_nothing() = runBlocking<Unit> {
        var dials = 0
        val hooks = hooks(SoftKeys(biometric = false), Host(), ask = { null })
        for (need in listOf(UnlockNeed.PASSWORD, UnlockNeed.KEY)) {
            val dialed = dialUnlocked(hooks, hostFp, row(need)) { dials++; HostServices.Cancelled }
            assertEquals(HostServices.Cancelled, dialed)
        }
        // The biometric sheet dismissed: the password prompt, and its Cancel, still dial nothing.
        val keyed = SoftKeys(biometric = true).apply { create(hostFp, "ada") }
        val asked = mutableListOf<PasswordAsk>()
        val sheetless = hooks(keyed, Host(), ask = { asked += it; null }, sign = { null })
        dialUnlocked(sheetless, hostFp, row(UnlockNeed.KEY)) { dials++; HostServices.Cancelled }
        assertEquals(0, dials)
        assertEquals(1, asked.size)
    }

    @Test
    fun a_locked_close_reopens_the_prompt_with_the_hosts_sentence() = runBlocking<Unit> {
        val typed = " pässwörd ".toCharArray()
        val answers = ArrayDeque(listOf(PasswordAnswer(typed, switchOn = true), null))
        val asked = mutableListOf<PasswordAsk>()
        val hooks = hooks(SoftKeys(biometric = false), Host(), ask = { asked += it; answers.removeFirst() })
        val sent = mutableListOf<Unlock?>()
        val refusals = ArrayDeque(listOf(locked(""), locked("Wrong password.")))
        val dialed = dialUnlocked(hooks, hostFp, row(UnlockNeed.NONE)) { u ->
            sent += u?.let { Unlock(it.kind, it.secret.copyOf(), it.staySignedIn, it.enrollKey) }
            refusals.removeFirst()
        }
        assertEquals(HostServices.Cancelled, dialed)
        // Bare first; an empty sentence asks plainly; the host's own sentence shows as the error.
        assertNull(sent[0])
        assertEquals(listOf(null, Say.Text("Wrong password.")), asked.map { it.error })
        assertEquals("Ada", asked[0].profileName)
        val password = sent[1]!!
        assertEquals(UNLOCK_PASSWORD, password.kind)
        // Sent as typed, never trimmed; the typed copy is wiped.
        assertArrayEquals(" pässwörd ".toByteArray(Charsets.UTF_8), password.secret)
        assertTrue(typed.all { it == '\u0000' })
        // No biometrics here: the switch stays signed in and enrols nothing.
        assertTrue(password.staySignedIn)
        assertNull(password.enrollKey)
        assertEquals(2, sent.size)
    }

    @Test
    fun the_switch_enrols_a_key_where_biometrics_are_enrolled() = runBlocking<Unit> {
        val keys = SoftKeys(biometric = true)
        val hooks = hooks(keys, Host(), ask = { PasswordAnswer("pw".toCharArray(), switchOn = true) })
        var sent: Unlock? = null
        val wiped = mutableListOf<Unlock>()
        dialUnlocked(hooks, hostFp, row(UnlockNeed.PASSWORD)) { u ->
            sent = Unlock(u!!.kind, u.secret.copyOf(), u.staySignedIn, u.enrollKey)
            wiped += u
            HostServices.Cancelled
        }
        val enrol = sent!!
        assertFalse(enrol.staySignedIn)
        assertEquals(ENROLL_P256, enrol.enrollAlg)
        assertArrayEquals(p256Point(keys.pair!!.public.encoded), enrol.enrollKey)
        // The secret is wiped once the dial returns.
        assertTrue(wiped.single().secret.all { it == 0.toByte() })
        // A key that can't be made enrols nothing; the password still goes.
        keys.failCreate = true
        dialUnlocked(hooks, hostFp, row(UnlockNeed.PASSWORD)) { u ->
            assertNull(u!!.enrollKey)
            assertEquals(0, u.enrollAlg)
            HostServices.Cancelled
        }
    }

    @Test
    fun the_host_asking_for_the_password_deletes_a_stale_key() = runBlocking<Unit> {
        val keys = SoftKeys(biometric = true).apply { create(hostFp, "ada") }
        val hooks = hooks(keys, Host(), ask = { null })
        dialUnlocked(hooks, hostFp, row(UnlockNeed.NONE)) { HostServices.Cancelled }
        assertTrue("a running desktop keeps the key", keys.has(hostFp, "ada"))
        dialUnlocked(hooks, hostFp, row(UnlockNeed.PASSWORD)) { HostServices.Cancelled }
        assertFalse(keys.has(hostFp, "ada"))
    }

    @Test
    fun an_invalidated_key_falls_to_the_password_prompt() = runBlocking<Unit> {
        val keys = SoftKeys(biometric = true).apply { create(hostFp, "ada"); invalidated = true }
        val host = Host()
        val asked = mutableListOf<PasswordAsk>()
        val hooks = hooks(keys, host, ask = { asked += it; null })
        dialUnlocked(hooks, hostFp, row(UnlockNeed.KEY)) { error("dialed") }
        assertFalse(keys.has(hostFp, "ada"))
        assertTrue(host.challenged.isEmpty())
        // The prompt offers to enrol again.
        assertTrue(asked.single().biometric)
    }

    @Test
    fun an_unpinned_or_promptless_dial_goes_bare() = runBlocking<Unit> {
        val hooks = hooks(SoftKeys(biometric = false), Host(), ask = { error("prompted") })
        val bare = mutableListOf<Unlock?>()
        dialUnlocked(hooks, "", row(UnlockNeed.PASSWORD)) { bare += it; locked("") }
        dialUnlocked(null, hostFp, row(UnlockNeed.PASSWORD)) { bare += it; locked("") }
        assertEquals(listOf<Unlock?>(null, null), bare)
    }

    @Test
    fun a_too_long_password_is_asked_again() = runBlocking<Unit> {
        val answers = ArrayDeque(listOf(PasswordAnswer(CharArray(513) { 'a' }, false), null))
        val asked = mutableListOf<PasswordAsk>()
        val hooks = hooks(SoftKeys(biometric = false), Host(), ask = { asked += it; answers.removeFirst() })
        dialUnlocked(hooks, hostFp, row(UnlockNeed.PASSWORD)) { error("dialed") }
        assertEquals(listOf(null, say(R.string.unlock_error_too_long)), asked.map { it.error })
    }

    @Test
    fun a_redial_the_host_no_longer_admits_goes_to_the_key_unlock() = runBlocking<Unit> {
        // A redial knows only the saved pick, and goes bare first.
        val keys = SoftKeys(biometric = true).apply { create(hostFp, "ada") }
        val hooks = hooks(keys, Host(), ask = { error("the key comes first") })
        val sent = mutableListOf<Int?>()
        val replies = ArrayDeque(listOf(locked(""), HostServices.Cancelled))
        dialUnlocked(hooks, hostFp, savedRow("ada", null)) { u -> sent += u?.kind; replies.removeFirst() }
        assertEquals(listOf(null, UNLOCK_KEY_PROOF), sent)
        // Without a key here, the same refusal opens the password prompt.
        val asked = mutableListOf<PasswordAsk>()
        val prompted = hooks(SoftKeys(biometric = false), Host(), ask = { asked += it; null })
        dialUnlocked(prompted, hostFp, savedRow("ada", null)) { locked("") }
        assertEquals(1, asked.size)
    }

    @Test
    fun pending_on_an_unlock_dial_is_the_desktop_starting() {
        val unlocking = ConnectAttempt("Desk").apply { unlocking = "Ada" }
        unlocking.hostPending()
        assertEquals("Ada", unlocking.starting)
        assertFalse(unlocking.awaitingApproval)
        // The next dial starts plain again; a bare dial that is parked waits for approval.
        unlocking.unlocking = null
        assertNull(unlocking.starting)
        unlocking.hostPending()
        assertTrue(unlocking.awaitingApproval)
    }

    @Test
    fun a_profile_locked_close_reads_as_a_locked_profile() {
        assertEquals(R.string.reject_profile_locked, ConnectErrors.connectMessage(PROFILE_LOCKED, requestAccess = false))
    }

    @Test
    fun a_challenge_is_exactly_its_two_hex_fields() {
        val ok = UnlockChallenge.parse("""{"challenge":"${"01".repeat(32)}","seat":"$vectorSeat"}""")!!
        assertArrayEquals(ByteArray(32) { 1 }, ok.challenge)
        assertEquals(vectorSeat, ok.seat)
        for (bad in listOf(
            """{"challenge":"${"0A".repeat(32)}","seat":"$vectorSeat"}""",
            """{"challenge":"${"01".repeat(31)}","seat":"$vectorSeat"}""",
            """{"challenge":"${"01".repeat(32)}","seat":"${vectorSeat}00"}""",
            """{"seat":"$vectorSeat"}""",
            "not json",
            "{\"pad\":\"${"x".repeat(2000)}\"}",
        )) {
            assertNull(bad, UnlockChallenge.parse(bad))
        }
        assertEquals("Unlock(kind=1, 3 bytes)", Unlock(UNLOCK_PASSWORD, "abc".toByteArray()).toString())
    }

    @Test
    fun a_pin_profile_asks_for_its_pin_and_sends_its_ascii_digits() = runBlocking<Unit> {
        val short = "１２３".toCharArray()
        val wide = "２４６８".toCharArray()
        val answers = ArrayDeque(listOf(PasswordAnswer(short, false), PasswordAnswer(wide, switchOn = true), null))
        val asked = mutableListOf<PasswordAsk>()
        val hooks = hooks(SoftKeys(biometric = false), Host(), ask = { asked += it; answers.removeFirst() })
        val sent = mutableListOf<Unlock>()
        val refusals = ArrayDeque(listOf(locked("That isn't this profile's PIN.")))
        val dialed = dialUnlocked(hooks, hostFp, row(UnlockNeed.PIN)) { u ->
            sent += Unlock(u!!.kind, u.secret.copyOf(), u.staySignedIn, u.enrollKey)
            refusals.removeFirst()
        }
        assertEquals(HostServices.Cancelled, dialed)
        assertTrue(asked.all { it.pin })
        // A short PIN is asked again in the prompt's words; the host's refusal shows as it came.
        assertEquals(
            listOf(null, say(R.string.unlock_pin_error_shape), Say.Text("That isn't this profile's PIN.")),
            asked.map { it.error },
        )
        val pin = sent.single()
        assertEquals(UNLOCK_PASSWORD, pin.kind)
        assertArrayEquals("2468".toByteArray(), pin.secret)
        assertTrue(pin.staySignedIn)
        assertTrue(short.all { it == '\u0000' } && wide.all { it == '\u0000' })
    }

    @Test
    fun a_pin_key_profile_signs_the_message_bound_to_its_id() = runBlocking<Unit> {
        val keys = SoftKeys(biometric = true).apply { create(hostFp, "ada") }
        val host = Host(UnlockChallenge(vectorChallenge, null, "ada"))
        val sent = mutableListOf<Unlock?>()
        dialUnlocked(hooks(keys, host, ask = { error("no prompt") }), hostFp, row(UnlockNeed.PIN_KEY)) { u ->
            sent += u?.let { Unlock(it.kind, it.secret.copyOf()) }
            HostServices.Cancelled
        }
        val proof = sent.single()!!
        assertEquals(UNLOCK_KEY_PROOF, proof.kind)
        assertEquals(listOf(true), host.sheets)
        val verify = Signature.getInstance("SHA256withECDSA").apply { initVerify(keys.pair!!.public) }
        verify.update(profileMessage(vectorChallenge, "ada", deviceFp, boxFp))
        assertTrue(verify.verify(proof.secret))
    }

    @Test
    fun a_challenge_for_another_profile_signs_nothing_and_asks_for_the_pin() = runBlocking<Unit> {
        val keys = SoftKeys(biometric = true).apply { create(hostFp, "ada") }
        val host = Host(UnlockChallenge(vectorChallenge, null, "ben"))
        val asked = mutableListOf<PasswordAsk>()
        dialUnlocked(hooks(keys, host, ask = { asked += it; null }), hostFp, row(UnlockNeed.PIN_KEY)) { error("dialed") }
        assertTrue("no sheet for another profile's challenge", host.sheets.isEmpty())
        assertTrue(asked.single().pin)
    }

    @Test
    fun the_host_asking_for_the_pin_deletes_a_stale_key() = runBlocking<Unit> {
        val keys = SoftKeys(biometric = true).apply { create(hostFp, "ada") }
        dialUnlocked(hooks(keys, Host(), ask = { null }), hostFp, row(UnlockNeed.PIN)) { error("dialed") }
        assertFalse(keys.has(hostFp, "ada"))
    }

    @Test
    fun a_bare_refusal_asks_in_the_form_of_the_fresh_row() = runBlocking<Unit> {
        // A redial knows only the saved pick; the host's fresh row says the profile has a PIN.
        val host = Host().apply { fresh = ListedProfile(id = "ada", displayName = "Ada", unlock = UnlockNeed.PIN) }
        val asked = mutableListOf<PasswordAsk>()
        dialUnlocked(hooks(SoftKeys(biometric = false), host, ask = { asked += it; null }), hostFp, savedRow("ada", null)) {
            locked("")
        }
        assertTrue(asked.single().pin)
        assertEquals("Ada", asked.single().profileName)
    }

    private fun locked(said: String) = HostServices.Failed(PROFILE_LOCKED, said)

    private fun p256(): KeyPair =
        KeyPairGenerator.getInstance("EC").apply { initialize(ECGenParameterSpec("secp256r1")) }.generateKeyPair()

    private fun hooks(
        keys: UnlockKeys,
        host: Host,
        ask: suspend (PasswordAsk) -> PasswordAnswer?,
        sign: suspend (Signature) -> Signature? = { it },
    ) = UnlockHooks(
        keys = keys,
        deviceFp = deviceFp,
        askPassword = ask,
        challenge = { id -> host.challenged += id; host.answer },
        message = { c, s, d, b ->
            assertArrayEquals(deviceFp, d)
            assertArrayEquals(boxFp, b)
            contractMessage(c, s, d, b)
        },
        profileMessage = { c, id, d, b -> profileMessage(c, id, d, b) },
        authenticate = { signature, _, pin -> host.sheets += pin; sign(signature) },
        row = { id -> host.fresh?.takeIf { it.id == id } },
    )

    /** The contract's 187-byte form for a locked profile: its id zero-padded to 64 bytes. */
    private fun profileMessage(challenge: ByteArray, id: String, device: ByteArray, box: ByteArray) =
        "punktfunk-profile-unlock-v1".toByteArray() + challenge + id.toByteArray().copyOf(64) + device + box

    /** What the scripted host answers: a [answer] challenge, and [fresh] as the refused profile's row. */
    private class Host(val answer: UnlockChallenge = UnlockChallenge(ByteArray(32) { (it + 1).toByte() }, "0123456789abcdef0123456789abcdef")) {
        val challenged = mutableListOf<String>()
        val sheets = mutableListOf<Boolean>()
        var fresh: ListedProfile? = null
    }

    /** A software P-256 key in place of the Keystore's: the same calls, no biometric behind it. */
    private inner class SoftKeys(private val biometric: Boolean) : UnlockKeys {
        var pair: KeyPair? = null
        var failCreate = false
        var invalidated = false
        private var holds: Pair<String, String>? = null

        override fun canEnroll() = biometric
        override fun has(hostFp: String, profile: String) = holds == hostFp to profile

        override fun create(hostFp: String, profile: String): ByteArray? {
            if (failCreate) return null
            pair = p256()
            holds = hostFp to profile
            return p256Point(pair!!.public.encoded)
        }

        override fun delete(hostFp: String, profile: String) {
            if (has(hostFp, profile)) holds = null
        }

        override fun signer(hostFp: String, profile: String): Signature? {
            if (invalidated) {
                delete(hostFp, profile)
                return null
            }
            return Signature.getInstance("SHA256withECDSA").apply { initSign(pair!!.private) }
        }
    }
}
