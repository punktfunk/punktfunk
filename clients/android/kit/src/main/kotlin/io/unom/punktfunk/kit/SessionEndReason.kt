package io.unom.punktfunk.kit

/**
 * Why a stream session ended — the Kotlin mirror of `punktfunk_core::client::PunktfunkEndReason`,
 * read via [NativeBridge.nativeEndReason].
 *
 * The distinction that matters to a UI is **normal vs alarming**, and it is not a spectrum: a
 * player quitting their game and a host falling off the network both arrive as "the session
 * ended". Without a way to tell them apart, one message serves all of them — and it would be the
 * alarming one ("Connection lost — the host may be asleep"), shown to players who had just quit
 * their own game.
 *
 * Ordinals are an ABI contract with the Rust side: append only, never renumber.
 */
enum class SessionEndReason {
    /** Not ended, or ended before a reason could be observed. Also the fallback for an unknown value. */
    NONE,

    /** This client closed the session — the user pressed back or stop. Nothing to report. */
    LOCAL,

    /**
     * The host's launched game exited. A normal finish, and the one reason worth acting on: go back
     * to the library the title was launched from, so the next one is a tap away.
     */
    GAME_EXITED,

    /** The host ended the session deliberately (an operator "End", or it simply finished). Normal. */
    HOST_ENDED,

    /** The host closed reporting a failure of its own. Worth showing; the host's log has the detail. */
    HOST_ERROR,

    /**
     * The connection died rather than being closed: idle timeout, reset, the network going away.
     * This — and only this — is the "the host may be asleep, wake it" case.
     */
    LOST;

    /**
     * Is this an ordinary outcome rather than something to alarm the user about?
     *
     * The question nearly every caller actually asks. [LOCAL], [GAME_EXITED] and [HOST_ENDED] were
     * all meant to happen. [NONE] counts as normal — no evidence of trouble is not evidence of it.
     */
    val isNormal: Boolean
        get() = this != HOST_ERROR && this != LOST

    /** Milliseconds before redial [attempt] (0-based), on the schedule every client shares; null = stay ended. */
    fun redialDelayMs(attempt: Int): Long? =
        NativeBridge.nativeRedialDelayMs(ordinal, attempt).takeIf { it >= 0 }

    companion object {
        /**
         * Decode the JNI byte. An unrecognized value becomes [NONE] rather than throwing: this
         * crosses an ABI where the native side may be newer than this code.
         */
        fun fromNative(v: Int): SessionEndReason = entries.getOrNull(v) ?: NONE
    }
}
