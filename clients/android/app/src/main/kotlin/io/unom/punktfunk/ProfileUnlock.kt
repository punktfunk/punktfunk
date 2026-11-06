package io.unom.punktfunk

import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.UnlockNeed
import io.unom.punktfunk.kit.say
import java.nio.CharBuffer
import java.nio.charset.CharacterCodingException
import java.security.Signature
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject

/** `kind` 1 of the wire's `ProfileUnlock`: the password as typed. */
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
 * The password prompt a dial waits on: the profile's name, whether its switch makes an unlock key
 * ([biometric]) or stays signed in, and the host's sentence after a refusal. [answer] completes
 * with what was typed, or `null` for Cancel.
 */
class PasswordAsk(val profileName: String, val biometric: Boolean, val error: Say?) {
    val answer = CompletableDeferred<PasswordAnswer?>()
}

/** The typed password and the switch. The dial wipes [password] once it has encoded it. */
class PasswordAnswer(val password: CharArray, val switchOn: Boolean) {
    override fun toString() = "PasswordAnswer(${password.size} chars)"
}

/** The host's answer to `POST /api/v1/profiles/{id}/unlock-challenge`. */
class UnlockChallenge(val challenge: ByteArray, val seat: String) {
    companion object {
        private val HEX64 = Regex("[0-9a-f]{64}")
        private val HEX32 = Regex("[0-9a-f]{32}")

        /** `{"challenge": "<64 hex>", "seat": "<32 hex>"}`; `null` for anything else. */
        fun parse(body: String): UnlockChallenge? {
            if (body.length > 1024) return null
            val o = runCatching { JSONObject(body) }.getOrNull() ?: return null
            val challenge = o.optString("challenge").takeIf { HEX64.matches(it) } ?: return null
            val seat = o.optString("seat").takeIf { HEX32.matches(it) } ?: return null
            return hexBytes(challenge, 32)?.let { UnlockChallenge(it, seat) }
        }
    }
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
 * What a dial to a profile on a person's own account needs from its shell: the keys, this
 * device's certificate SHA-256, the password prompt, the host's challenge, the 152-byte message
 * of a key proof, and the biometric sheet that releases a [Signature].
 */
class UnlockHooks(
    val keys: UnlockKeys,
    val deviceFp: ByteArray,
    val askPassword: suspend (PasswordAsk) -> PasswordAnswer?,
    val challenge: suspend (profileId: String) -> UnlockChallenge?,
    val message: (challenge: ByteArray, seat: String, deviceFp: ByteArray, boxFp: ByteArray) -> ByteArray?,
    val authenticate: suspend (Signature, profileName: String) -> Signature?,
)

/**
 * Dials [profile] on the box pinned to [hostFp] with the unlock it needs: none, a proof from this
 * device's key, else the password. A key that can't sign falls to the prompt, and the host asking
 * for the password deletes the key it no longer holds. A `profile-locked` close of a bare dial
 * goes to the key unlock when this device holds the key, every other one to the prompt with the
 * host's sentence; only the player's answer dials again. A cancelled prompt dials nothing and
 * answers [HostServices.Cancelled]. Without [hooks] or a pin this is one bare dial.
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
    val keys = hooks.keys
    if (profile.unlock == UnlockNeed.PASSWORD && io { keys.has(fp, profile.id) }) {
        io { keys.delete(fp, profile.id) }
    }
    var next = profile.unlock
    var error: String? = null
    while (true) {
        val unlock = when (next) {
            UnlockNeed.NONE -> null
            UnlockNeed.KEY -> keyProof(hooks, fp, box, profile)
                ?: password(hooks, fp, profile, error)
                ?: return HostServices.Cancelled
            UnlockNeed.PASSWORD -> password(hooks, fp, profile, error) ?: return HostServices.Cancelled
        }
        val dialed = try {
            dial(unlock)
        } finally {
            unlock?.wipe()
        }
        if (dialed !is HostServices.Failed || dialed.token != PROFILE_LOCKED) return dialed
        val keyNext = next == UnlockNeed.NONE && io { keys.has(fp, profile.id) }
        next = if (keyNext) UnlockNeed.KEY else UnlockNeed.PASSWORD
        error = dialed.said.ifEmpty { null }
    }
}

/** Asks until the password fits the wire or the player cancels (`null`). */
private suspend fun password(hooks: UnlockHooks, fp: String, profile: ListedProfile, refused: String?): Unlock? {
    val biometric = io { hooks.keys.canEnroll() }
    var error: Say? = refused?.let { Say.Text(it) }
    while (true) {
        val answer = hooks.askPassword(PasswordAsk(profile.displayName, biometric, error)) ?: return null
        val secret = try {
            utf8(answer.password)
        } catch (_: CharacterCodingException) {
            null
        } finally {
            answer.password.fill('\u0000')
        }
        error = when {
            secret == null -> say(R.string.unlock_error_bad_char)
            secret.size > MAX_PASSWORD_BYTES -> say(R.string.unlock_error_too_long)
            secret.isEmpty() -> null
            else -> {
                // A key that can't be made enrols nothing; the password still goes.
                val enroll = if (answer.switchOn && biometric) io { hooks.keys.create(fp, profile.id) } else null
                return Unlock(UNLOCK_PASSWORD, secret, staySignedIn = answer.switchOn && !biometric, enrollKey = enroll)
            }
        }
        secret?.fill(0)
    }
}

/** A signature over the host's challenge, or `null` when there is no key or it didn't sign. */
private suspend fun keyProof(hooks: UnlockHooks, fp: String, box: ByteArray, profile: ListedProfile): Unlock? {
    val keys = hooks.keys
    val signer = io { if (keys.has(fp, profile.id)) keys.signer(fp, profile.id) else null } ?: return null
    val challenge = hooks.challenge(profile.id) ?: return null
    val message = hooks.message(challenge.challenge, challenge.seat, hooks.deviceFp, box) ?: return null
    val signed = hooks.authenticate(signer, profile.displayName) ?: return null
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
