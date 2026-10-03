package io.unom.punktfunk.kit

/**
 * Per-client access grants — the Kotlin mirror of `punktfunk_core::quic::access` (bit-for-bit;
 * `design/per-client-access.md` §3), read per session via [NativeBridge.nativeAccessState].
 *
 * The host is the only enforcer: everything gated on these bits client-side is courtesy UX over
 * the same vocabulary — don't capture what can't land (a keyboard that silently does nothing is
 * the failure mode this exists to prevent), and say what this session is (the stream's Access
 * chip). The user-facing word is **"Access"**; the preset labels are *derived* from the mask,
 * never stored, so they can't drift from what the host actually granted.
 */
object SessionAccess {
    /** Controller input — gamepad events, rich pad input, pad audio, rumble return. */
    const val GAMEPAD = 1 shl 0

    /** Pointing input — mouse rel/abs + buttons, scroll, touch, and the pen plane. */
    const val POINTER = 1 shl 1

    /** Key input — key down/up and IME-committed text. */
    const val KEYBOARD = 1 shl 2

    /** Shared clipboard (ANDed into the host's clipboard policy). */
    const val CLIPBOARD = 1 shl 3

    /** Mic injection — the uplink plane + the per-session mic attach. */
    const val MIC = 1 shl 4

    /** Library launch (`Hello.launch`). */
    const val LAUNCH = 1 shl 5

    /** Host power — the `power.*` host actions (`design/host-actions.md`); route-gated, never input. */
    const val POWER = 1 shl 6

    /** Pause and remove a title's download (`design/plugin-downloads.md`); route-gated, never input. */
    const val MANAGE_GAMES = 1 shl 7

    /** Every defined grant — full control, and what an old host's Welcome decodes to. */
    const val ALL = GAMEPAD or POINTER or KEYBOARD or CLIPBOARD or MIC or LAUNCH or POWER or MANAGE_GAMES

    /** `ALL` before POWER existed (hosts ≤ 0.32.x) — see [normalizeLegacyFull]. */
    private const val ALL_PRE_POWER = GAMEPAD or POINTER or KEYBOARD or CLIPBOARD or MIC or LAUNCH

    /** `ALL` before MANAGE_GAMES existed — see [normalizeLegacyFull]. */
    private const val ALL_PRE_MANAGE = ALL_PRE_POWER or POWER

    /**
     * The legacy-full read rule (host-actions §4.3): exactly an older full mask (an old
     * host's "Full control") reads as the current [ALL], so it labels "Full control", not
     * "Custom". Any other mask passes through.
     */
    fun normalizeLegacyFull(grants: Int): Int =
        if (grants == ALL_PRE_POWER || grants == ALL_PRE_MANAGE) ALL else grants

    /**
     * The preset name a mask displays as — §3.2's rule: three levels people actually reason
     * about, "Custom" for any other combination, never a raw bit list.
     */
    fun label(grants: Int): String = when (normalizeLegacyFull(grants) and ALL) {
        ALL -> "Full control"
        GAMEPAD -> "Controller only"
        0 -> "View only"
        else -> "Custom"
    }

    /**
     * Compact time-left wording for the Access chip ("1 h 58 m", "12 m", "45 s") — hours and
     * minutes once the span has them, bare seconds only under a minute (the final countdown).
     */
    fun remainingLabel(secs: Int): String {
        val h = secs / 3600
        val m = (secs % 3600) / 60
        return when {
            h > 0 && m > 0 -> "$h h $m m"
            h > 0 -> "$h h"
            m > 0 -> "$m m"
            else -> "${secs.coerceAtLeast(0)} s"
        }
    }
}
