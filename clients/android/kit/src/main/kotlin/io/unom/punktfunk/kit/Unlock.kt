package io.unom.punktfunk.kit

import org.json.JSONObject

/**
 * Opening a profile on a person's own account or a locked one: whether a dial prompts, signs with
 * this device's unlock key or goes as it is, what the PIN field sends, and what the host's
 * challenge names. A port of `pf-client-core`'s `unlock.rs` rules;
 * `clients/shared/profile-unlock-vectors.json` pins them in every client.
 */

/** What a shell does before it dials a profile. */
enum class UnlockGate {
    /** Dial with no unlock. The host may still close with `profile-locked`. */
    DIAL,

    /** Sign the host's challenge with this device's key. No proof falls to [PROMPT]. */
    KEY,

    /** The prompt, in [UnlockNeed.pin]'s form. */
    PROMPT,
}

/** The gate for a profile listed with [need]. [holdsKey]: this device has its unlock key. */
fun beforeDial(need: UnlockNeed, holdsKey: Boolean): UnlockGate = when {
    need == UnlockNeed.NONE -> UnlockGate.DIAL
    holdsKey && (need == UnlockNeed.KEY || need == UnlockNeed.PIN_KEY) -> UnlockGate.KEY
    else -> UnlockGate.PROMPT
}

/** The host asks for the password or the PIN from a device that holds a key: it holds none for it. */
fun staleKey(need: UnlockNeed, holdsKey: Boolean): Boolean =
    holdsKey && (need == UnlockNeed.PASSWORD || need == UnlockNeed.PIN)

/**
 * Where a `profile-locked` close leads, from the profile's fresh [need]. A refused unlock ([sent])
 * reopens the prompt; a dial that carried none may unlock with this device's key. Never a dial.
 */
fun afterRefusal(need: UnlockNeed, holdsKey: Boolean, sent: Boolean): UnlockGate =
    if (!sent && beforeDial(need, holdsKey) == UnlockGate.KEY) UnlockGate.KEY else UnlockGate.PROMPT

/** How many digits a profile's PIN has. */
val PIN_LEN = 4..8

/**
 * [typed] as the PIN the wire carries: [PIN_LEN] ASCII digits, a full-width digit read as its
 * ASCII one. `null` for anything else, and [pinError] says why.
 */
fun pinDigits(typed: CharArray): ByteArray? {
    if (typed.size !in PIN_LEN) return null
    val pin = ByteArray(typed.size)
    for (i in typed.indices) {
        pin[i] = when (val c = typed[i]) {
            in '0'..'9' -> c.code
            in '０'..'９' -> c.code - 0xFF10 + '0'.code
            else -> {
                pin.fill(0)
                return null
            }
        }.toByte()
    }
    return pin
}

/** The line under the PIN field when [pinDigits] refused what was typed, [empty] or not. */
fun pinError(empty: Boolean): Say =
    say(if (empty) R.string.unlock_pin_error_empty else R.string.unlock_pin_error_shape)

/** An id `enumerate` can list: 1 to 64 of `[A-Za-z0-9_-]`. It names a URL path and a message. */
fun isProfileId(id: String): Boolean = PROFILE_ID.matches(id)

private val PROFILE_ID = Regex("[A-Za-z0-9_-]{1,64}")

/**
 * The host's answer to `POST /api/v1/profiles/{id}/unlock-challenge`: 32 fresh bytes and what the
 * signed message names, a [seat] (a person's own account) or a locked [profile]. One is set.
 */
class UnlockChallenge(val challenge: ByteArray, val seat: String?, val profile: String? = null) {
    companion object {
        private val HEX64 = Regex("[0-9a-f]{64}")
        private val HEX32 = Regex("[0-9a-f]{32}")

        /** `{challenge, seat}` or `{challenge, profile}`; `null` for both, neither, or any other shape. */
        fun parse(body: String): UnlockChallenge? {
            if (body.length > 1024) return null
            val o = runCatching { JSONObject(body) }.getOrNull() ?: return null
            val hex = o.optString("challenge").takeIf { HEX64.matches(it) } ?: return null
            val challenge = ByteArray(32) { hex.substring(it * 2, it * 2 + 2).toInt(16).toByte() }
            val seat = o.opt("seat").takeUnless { it == JSONObject.NULL }
            val profile = o.opt("profile").takeUnless { it == JSONObject.NULL }
            return when {
                profile == null && seat is String && HEX32.matches(seat) -> UnlockChallenge(challenge, seat)
                seat == null && profile is String && isProfileId(profile) -> UnlockChallenge(challenge, null, profile)
                else -> null
            }
        }
    }
}
