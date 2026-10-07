package io.unom.punktfunk

import io.unom.punktfunk.kit.NativeBridge

/**
 * Cause-specific user-facing messages for failed pair/connect attempts, keyed on the stable
 * machine token from [NativeBridge.nativeTakeLastError]. One vocabulary for both the PIN
 * ceremony and the request-access (delegated approval) path, so a dead network path is never
 * reported as "wrong PIN" and an operator denial is never reported as a timeout — the exact
 * collapse behind more than one support thread.
 */
object ConnectErrors {
    /** Message for a failed SPAKE2 PIN ceremony ([NativeBridge.nativePair] returned `""`). */
    fun pairMessage(token: String): String = when (token) {
        "crypto" -> "Wrong PIN — check the PIN on the host's Pairing page and try again."
        else -> shared(token) ?: transport(token)
    }

    /**
     * Message for a failed connect / request-access ([NativeBridge.nativeConnect] returned `0`).
     * [requestAccess] tunes the fallback wording for the delegated-approval path.
     */
    fun connectMessage(token: String, requestAccess: Boolean): String =
        shared(token) ?: when (token) {
            "crypto" ->
                "The host's identity doesn't match the saved fingerprint — re-pair with this host."
            "timeout", "io", "" ->
                if (requestAccess) {
                    "The request never reached the host, or nobody approved it in time — " +
                        "check the network path (no VPN, no guest-Wi-Fi isolation) and the " +
                        "host's console."
                } else {
                    transport(token)
                }
            else -> "Couldn't connect — check the host's address and port."
        }

    /** The host's typed rejection reasons — identical wording across every punktfunk client. */
    private fun shared(token: String): String? = when (token) {
        "not-armed" ->
            "Pairing isn't armed on the host — arm it on the host's Pairing page, then try again."
        "bound-other" ->
            "The host's pairing window is armed for a different device — arm it for this one."
        "rate-limited" -> "Too many pairing attempts — wait a couple of seconds and try again."
        "identity-required" ->
            "The host requires pairing — pair this device (PIN or request access) first."
        "denied" -> "The host declined this device's request."
        "approval-timeout" ->
            "Nobody approved the request on the host in time — approve this device in the " +
                "host's console or web UI, then request access again."
        "superseded" ->
            "A newer request from this device replaced this one — approve the latest request " +
                "on the host."
        "wire-version" -> "Client and host versions don't match — update both to the same release."
        "profile-unknown" -> "That profile is gone from this host. Pick another one."
        "no-seat" -> "All seats are taken."
        "seat-occupied" -> "Someone is already playing as that profile."
        "seat-unavailable" -> "That profile can't play on this host right now."
        "busy" -> "The host is busy with another session."
        "access-expired" ->
            "Your access to this host has expired — ask the host's owner to grant it again."
        "launch-not-permitted" ->
            "This device's access doesn't include launching games — connect to the desktop, " +
                "or ask the host's owner."
        // A host power action (design/host-actions.md) ended the session deliberately.
        // Without this arm it falls through to the generic failure, and sleeping your own
        // host from the couch reads as a crash.
        "host-power" ->
            "The host is going to sleep or shutting down — wake it when you want to play again."
        // The host accepted the connection and then failed to bring the stream up: no encoder
        // for the codec, pf-vdisplay missing, capture open failed. Everything host-side funnels
        // here, so without this arm every one of those reads as a network problem on a host that
        // answered and explained itself.
        "setup-failed" ->
            "The host accepted the connection but couldn't start the stream — the host's log " +
                "(web console → Log) has the cause."
        else -> null
    }

    /** Transport-level causes (nothing typed arrived from the host). */
    private fun transport(token: String): String = when (token) {
        "timeout" ->
            "The host didn't answer — check that this device and the host are on the same " +
                "network (no VPN on this device, no guest-Wi-Fi / AP isolation)."
        "io" ->
            "Couldn't reach the host — check that this device and the host are on the same " +
                "network (no VPN on this device, no guest-Wi-Fi / AP isolation)."
        else -> "Pairing failed — the host didn't answer or closed the connection."
    }
}
