package io.unom.punktfunk

import androidx.annotation.StringRes
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.R

/**
 * Cause-specific user-facing messages for failed pair/connect attempts, keyed on the stable
 * machine token from [NativeBridge.nativeTakeLastError]. One vocabulary for both the PIN
 * ceremony and the request-access (delegated approval) path, so a dead network path is never
 * reported as "wrong PIN" and an operator denial is never reported as a timeout — the exact
 * collapse behind more than one support thread. Each function returns a message id for the
 * caller to word.
 */
object ConnectErrors {
    /** Message for a failed SPAKE2 PIN ceremony ([NativeBridge.nativePair] returned `""`). */
    @StringRes
    fun pairMessage(token: String): Int = when (token) {
        "crypto" -> R.string.pair_wrong_pin
        else -> shared(token) ?: transport(token)
    }

    /**
     * Message for a failed connect / request-access ([NativeBridge.nativeConnect] returned `0`).
     * [requestAccess] tunes the fallback wording for the delegated-approval path.
     */
    @StringRes
    fun connectMessage(token: String, requestAccess: Boolean): Int =
        shared(token) ?: when (token) {
            "crypto" -> R.string.dial_fingerprint_changed
            "timeout", "io", "" ->
                if (requestAccess) R.string.error_request_failed else transport(token)
            else -> R.string.android_error_connect_check_address
        }

    /** The host's typed rejection reasons — identical wording across every punktfunk client. */
    @StringRes
    private fun shared(token: String): Int? = when (token) {
        "not-armed" -> R.string.reject_not_armed
        "bound-other" -> R.string.reject_bound_other
        "rate-limited" -> R.string.reject_rate_limited
        "identity-required" -> R.string.reject_identity_required
        "denied" -> R.string.reject_denied
        "approval-timeout" -> R.string.reject_approval_timeout
        "superseded" -> R.string.reject_superseded
        "wire-version" -> R.string.reject_wire_version
        "profile-unknown" -> R.string.reject_profile_unknown
        "no-seat" -> R.string.reject_no_seat
        "seat-occupied" -> R.string.reject_seat_occupied
        "seat-unavailable" -> R.string.reject_seat_unavailable
        // Where no prompt follows: a redial, or a host whose profiles didn't list.
        "profile-locked" -> R.string.reject_profile_locked
        "busy" -> R.string.reject_busy
        "access-expired" -> R.string.reject_access_expired
        "launch-not-permitted" -> R.string.reject_launch_not_permitted
        // A host power action ended the session on purpose: it is not a crash.
        "host-power" -> R.string.reject_host_power
        // The host accepted the connection, then could not bring the stream up: not a network fault.
        "setup-failed" -> R.string.reject_setup_failed
        else -> null
    }

    /** Transport-level causes (nothing typed arrived from the host). */
    @StringRes
    private fun transport(token: String): Int = when (token) {
        "timeout" -> R.string.error_host_timeout
        "io" -> R.string.error_host_unreachable
        else -> R.string.pair_failed
    }
}
