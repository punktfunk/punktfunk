package io.unom.punktfunk.kit.link

import io.unom.punktfunk.kit.security.KnownHost

/**
 * Where a bare launch opens: the host list, the default host's library, or its stream.
 *
 * The Kotlin third of the cross-client start-screen policy — `crates/client/pf-client-core/src/start.rs`
 * and `PunktfunkShared/StartScreen.swift` are the other two, and
 * `clients/shared/start-screen-vectors.json` holds all three to the same answers. It lives beside
 * [DeepLinks] because it resolves the same kind of reference: a stable host record id.
 */

/**
 * The `start_in` setting. Unknown reads as [HOSTS], the `library_view` convention: a value a
 * newer client wrote degrades, it never ends a launch.
 */
enum class StartIn(val stored: String, val label: String) {
    HOSTS("hosts", "Host list"),
    LIBRARY("library", "Library"),
    STREAM("stream", "Stream"),
    ;

    companion object {
        /** Stored → value, with the unknown fallback. */
        fun parse(raw: String?): StartIn =
            entries.firstOrNull { it.stored == raw } ?: HOSTS
    }
}

/** Where the resolved default came from, for the one log line a launch prints. */
enum class DefaultHostSource { EXPLICIT, DERIVED, NONE }

/** A resolved default host and how it was chosen. [host] is null when there is none. */
data class DefaultHost(val host: KnownHost?, val source: DefaultHostSource)

/** Where a bare launch opens, and which host it opens on. */
sealed interface StartScreen {
    object Hosts : StartScreen

    data class Library(val host: KnownHost) : StartScreen

    /**
     * The library plus one connect. One attempt: a refusal lands on the shelf underneath, and
     * nothing retries.
     */
    data class Stream(val host: KnownHost) : StartScreen

    companion object {
        /**
         * The host a bare launch opens on, and how it was chosen. The explicit [id] wins when it
         * names a paired record; else the sole paired record; else nothing.
         *
         * Paired-only, because a launch cannot pair — an unpaired host would be a dead landing.
         * A dangling id falls through to the derived rule, which is why forgetting a host needs
         * no write hook. Ids are compared case-insensitively: the Apple client stores an
         * uppercase `UUID.uuidString` under the same key name.
         */
        fun defaultHost(id: String?, hosts: List<KnownHost>): DefaultHost {
            val paired = hosts.filter { it.paired && it.fpHex.isNotEmpty() }
            val want = id?.lowercase()
            if (!want.isNullOrEmpty()) {
                paired.firstOrNull { it.id.lowercase() == want }
                    ?.let { return DefaultHost(it, DefaultHostSource.EXPLICIT) }
            }
            return if (paired.size == 1) {
                DefaultHost(paired[0], DefaultHostSource.DERIVED)
            } else {
                DefaultHost(null, DefaultHostSource.NONE)
            }
        }

        /** Where a bare launch opens. No default host degrades every value to the list. */
        fun resolve(startIn: String?, defaultHost: String?, hosts: List<KnownHost>): StartScreen {
            val host = defaultHost(defaultHost, hosts).host ?: return Hosts
            return when (StartIn.parse(startIn)) {
                StartIn.HOSTS -> Hosts
                StartIn.LIBRARY -> Library(host)
                StartIn.STREAM -> Stream(host)
            }
        }
    }
}

/**
 * The host this landing opens on, if any. An extension rather than an interface member: the two
 * data classes already carry a `host` of their own, and a member would have to be overridden in
 * both to say the same thing twice.
 */
val StartScreen.host: KnownHost?
    get() = when (this) {
        is StartScreen.Hosts -> null
        is StartScreen.Library -> host
        is StartScreen.Stream -> host
    }
