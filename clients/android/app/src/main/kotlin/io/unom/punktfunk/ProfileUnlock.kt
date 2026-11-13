package io.unom.punktfunk

import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.UnlockChallenge
import io.unom.punktfunk.kit.UnlockGate
import io.unom.punktfunk.kit.UnlockNeed
import io.unom.punktfunk.kit.afterRefusal
import io.unom.punktfunk.kit.beforeDial
import io.unom.punktfunk.kit.pinDigits
import io.unom.punktfunk.kit.pinError
import io.unom.punktfunk.kit.say
import io.unom.punktfunk.kit.staleKey
import java.nio.CharBuffer
import java.nio.charset.CharacterCodingException
import java.security.Signature
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/** `kind` 1 of the wire's `ProfileUnlock`: the password as typed, or the PIN's ASCII digits. */
const val UNLOCK_PASSWORD = 1

/** `kind` 2: a signature from this device's unlock key. */
const val UNLOCK_KEY_PROOF = 2

/** `enroll_alg` 1: ECDSA P-256 with SHA-256, the key as its 65-byte uncompressed point. */
const val ENROLL_P256 = 1

/** The wire's bound on a password, in UTF-8 bytes. */
const val MAX_PASSWORD_BYTES = 512

/** The host's close for a profile that needs an unlock it did not get, or refused. */
const val PROFILE_LOCKED = "profile-locked"

/**
 * One profile unlock for one dial: a password (UTF-8) or a signature in [secret], and with a
 * password the public key to enrol. [wipe] zeroes the secret; [toString] prints none of it.
 */
class Unlock(
    val kind: Int,
    val secret: ByteArray,
    val staySignedIn: Boolean = false,
    val enrollKey: ByteArray? = null,
) {
    val enrollAlg: Int get() = if (enrollKey != null) ENROLL_P256 else 0

    fun wipe() = secret.fill(0)

    override fun toString() = "Unlock(kind=$kind, ${secret.size} bytes)"
}

/**
 * The prompt a dial waits on: the profile's name, whether it asks for the profile's [pin] or the
 * account password, whether its switch makes an unlock key ([biometric]) or stays signed in, and
 * the host's sentence after a refusal. [answer] completes with what was typed, or `null` for Cancel.
 */
class PasswordAsk(val profileName: String, val biometric: Boolean, val error: Say?, val pin: Boolean = false) {
    val answer = CompletableDeferred<PasswordAnswer?>()
}

/** The typed password or PIN and the switch. The dial wipes [password] once it has encoded it. */
class PasswordAnswer(val password: CharArray, val switchOn: Boolean) {
    override fun toString() = "PasswordAnswer(${password.size} chars)"
}

/**
 * This device's profile unlock keys, one per (pinned host fingerprint, profile id): the Android
 * Keystore behind a strong biometric on a phone ([KeystoreUnlockKeys]), a software key in tests.
 */
interface UnlockKeys {
    /** A key made now would open for a strong biometric: one is enrolled. */
    fun canEnroll(): Boolean

    fun has(hostFp: String, profile: String): Boolean

    /** Makes the key, replacing any; its public key as the 65-byte uncompressed point, or `null`. */
    fun create(hostFp: String, profile: String): ByteArray?

    fun delete(hostFp: String, profile: String)

    /** `SHA256withECDSA` initialised for signing, or `null`. A key the OS invalidated is deleted. */
    fun signer(hostFp: String, profile: String): Signature?
}

/**
 * What a dial to a profile on a person's own account or a locked one needs from its shell: the
 * keys, this device's certificate SHA-256, the prompt, the profile's fresh `enumerate` row, the
 * host's challenge, the message of a key proof for a seat (152 bytes) or a locked profile (187),
 * and the biometric sheet that releases a [Signature].
 */
class UnlockHooks(
    val keys: UnlockKeys,
    val deviceFp: ByteArray,
    val askPassword: suspend (PasswordAsk) -> PasswordAnswer?,
    val challenge: suspend (profileId: String) -> UnlockChallenge?,
    val message: (challenge: ByteArray, seat: String, deviceFp: ByteArray, boxFp: ByteArray) -> ByteArray?,
    val profileMessage: (challenge: ByteArray, profileId: String, deviceFp: ByteArray, boxFp: ByteArray) -> ByteArray?,
    val authenticate: suspend (Signature, profileName: String, pin: Boolean) -> Signature?,
    val row: suspend (profileId: String) -> ListedProfile? = { null },
)

/**
 * Dials [profile] on the box pinned to [hostFp] with the unlock its [UnlockNeed] asks for, by the
 * shared gate ([beforeDial], [staleKey], [afterRefusal]). A key that can't sign falls to the
 * prompt. A `profile-locked` close reads the profile's fresh row, then signs or reopens the prompt
 * with the host's sentence; only the player's answer dials again. A cancelled prompt dials
 * nothing and answers [HostServices.Cancelled]. Without [hooks] or a pin this is one bare dial.
 */
suspend fun dialUnlocked(
    hooks: UnlockHooks?,
    hostFp: String,
    profile: ListedProfile?,
    dial: suspend (Unlock?) -> HostServices.Dialed,
): HostServices.Dialed {
    val fp = hostFp.lowercase()
    val box = hexBytes(fp, 32)
    if (hooks == null || profile == null || box == null) return dial(null)
    var row: ListedProfile = profile
    var gate = beforeDial(row.unlock, holds(hooks.keys, fp, row))
    var error: String? = null
    while (true) {
        val unlock = when (gate) {
            UnlockGate.DIAL -> null
            UnlockGate.KEY -> keyProof(hooks, fp, box, row)
                ?: prompt(hooks, fp, row, error)
                ?: return HostServices.Cancelled
            UnlockGate.PROMPT -> prompt(hooks, fp, row, error) ?: return HostServices.Cancelled
        }
        val dialed = try {
            dial(unlock)
        } finally {
            unlock?.wipe()
        }
        if (dialed !is HostServices.Failed || dialed.token != PROFILE_LOCKED) return dialed
        // Without a fresh row, a bare dial's refusal reads as a key profile where a key is held.
        row = hooks.row(row.id) ?: row.takeIf { it.unlock != UnlockNeed.NONE }
            ?: row.copy(unlock = if (io { hooks.keys.has(fp, row.id) }) UnlockNeed.KEY else UnlockNeed.PASSWORD)
        gate = afterRefusal(row.unlock, holds(hooks.keys, fp, row), sent = unlock != null)
        error = dialed.said.ifEmpty { null }
    }
}

/** This device holds [row]'s key, after deleting one the host signed out ([staleKey]). */
private suspend fun holds(keys: UnlockKeys, fp: String, row: ListedProfile): Boolean {
    if (row.unlock == UnlockNeed.NONE) return false
    val holds = io { keys.has(fp, row.id) }
    if (!staleKey(row.unlock, holds)) return holds
    io { keys.delete(fp, row.id) }
    return false
}

/** Asks until what was typed fits the wire, or the PIN's shape for a PIN, or the player cancels (`null`). */
private suspend fun prompt(hooks: UnlockHooks, fp: String, profile: ListedProfile, refused: String?): Unlock? {
    val biometric = io { hooks.keys.canEnroll() }
    val pin = profile.unlock.pin
    var error: Say? = refused?.let { Say.Text(it) }
    while (true) {
        val answer = hooks.askPassword(PasswordAsk(profile.displayName, biometric, error, pin)) ?: return null
        val empty = answer.password.isEmpty()
        val secret = try {
            if (pin) pinDigits(answer.password) else utf8(answer.password)
        } catch (_: CharacterCodingException) {
            null
        } finally {
            answer.password.fill('\u0000')
        }
        error = when {
            pin && secret == null -> pinError(empty)
            secret == null -> say(R.string.unlock_error_bad_char)
            secret.size > MAX_PASSWORD_BYTES -> say(R.string.unlock_error_too_long)
            secret.isEmpty() -> null
            else -> {
                // A key that can't be made enrols nothing; the password or PIN still goes.
                val enroll = if (answer.switchOn && biometric) io { hooks.keys.create(fp, profile.id) } else null
                return Unlock(UNLOCK_PASSWORD, secret, staySignedIn = answer.switchOn && !biometric, enrollKey = enroll)
            }
        }
        secret?.fill(0)
    }
}

/**
 * A signature over the message the host's challenge names, a seat's or this profile's, or `null`
 * when there is no key, the challenge names another profile, or it didn't sign.
 */
private suspend fun keyProof(hooks: UnlockHooks, fp: String, box: ByteArray, profile: ListedProfile): Unlock? {
    val keys = hooks.keys
    val signer = io { if (keys.has(fp, profile.id)) keys.signer(fp, profile.id) else null } ?: return null
    val c = hooks.challenge(profile.id) ?: return null
    val seat = c.seat
    val message = when {
        seat != null -> hooks.message(c.challenge, seat, hooks.deviceFp, box)
        c.profile == profile.id -> hooks.profileMessage(c.challenge, profile.id, hooks.deviceFp, box)
        else -> null
    } ?: return null
    val signed = hooks.authenticate(signer, profile.displayName, profile.unlock.pin) ?: return null
    val proof = runCatching { signed.update(message); signed.sign() }.getOrNull() ?: return null
    return Unlock(UNLOCK_KEY_PROOF, proof)
}

/** [chars] as UTF-8, never through a String. Throws on a lone surrogate. */
internal fun utf8(chars: CharArray): ByteArray {
    val buf = Charsets.UTF_8.newEncoder().encode(CharBuffer.wrap(chars))
    val out = ByteArray(buf.remaining())
    buf.get(out)
    if (buf.hasArray()) buf.array().fill(0)
    return out
}

/**
 * The 65-byte uncompressed point (`04‖X‖Y`) of a P-256 key from its X.509 SubjectPublicKeyInfo
 * DER, or `null` for any other curve or encoding.
 */
fun p256Point(spki: ByteArray): ByteArray? {
    if (spki.size != P256_SPKI_PREFIX.size + 65) return null
    if (!spki.copyOfRange(0, P256_SPKI_PREFIX.size).contentEquals(P256_SPKI_PREFIX)) return null
    return spki.copyOfRange(P256_SPKI_PREFIX.size, spki.size).takeIf { it[0] == 0x04.toByte() }
}

/** SEQUENCE { SEQUENCE { id-ecPublicKey, prime256v1 }, BIT STRING (65 bytes) } up to the point. */
private val P256_SPKI_PREFIX = hexBytes("3059301306072a8648ce3d020106082a8648ce3d030107034200", 26)!!

/** Lowercase hex of exactly [size] bytes, or `null`. */
internal fun hexBytes(hex: String, size: Int): ByteArray? {
    if (hex.length != size * 2 || hex.any { it !in '0'..'9' && it !in 'a'..'f' }) return null
    return ByteArray(size) { hex.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
}

private suspend fun <T> io(block: () -> T): T = withContext(Dispatchers.IO) { block() }
