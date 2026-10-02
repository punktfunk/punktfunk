package io.unom.punktfunk.kit.link

import io.unom.punktfunk.kit.security.KnownHost
import java.nio.ByteBuffer
import java.nio.charset.CodingErrorAction
import java.nio.charset.StandardCharsets

/**
 * The `punktfunk://` URL grammar (design/client-deep-links.md §2). A **port**, not a new design:
 * the Rust `crates/pf-client-core/src/deeplink.rs` is the reference, Swift keeps a third copy, and
 * all three are held together by `clients/shared/deeplink-vectors.json`, which each language's test
 * suite runs verbatim — so the three parsers cannot drift into three different security postures.
 *
 * ```text
 * punktfunk://connect/<host-ref>[?fp=<64-hex>][&host=<addr[:port]>][&launch=<id>]
 *                               [&preset=<ref>][&name=<label>]
 * ```
 *
 * The invariant the grammar exists to keep: **a URL may only ever do what a click on an existing
 * card could do, minus trust decisions.** So it carries *references* to things that already exist
 * on this device — a host record, a settings preset, a library id — and never values: no
 * resolution, no bitrate, no codec. A web page must not be able to shape a session beyond picking
 * among the user's own configurations. `pair` is deliberately not a route and never will be;
 * pairing stays an interactive ceremony.
 *
 * `pf://` parses as an alias so a hand-typed or legacy link still works, but nothing ever *emits*
 * or registers it.
 */
object DeepLinks {
    /** Hostile-input caps. Generous for a real link, small enough that a pasted megabyte stops here. */
    const val MAX_URL_LEN = 2048
    const val MAX_HOST_REF_LEN = 128
    const val MAX_LAUNCH_LEN = 128
    const val MAX_PRESET_LEN = 64
    const val MAX_NAME_LEN = 64

    /** The default native port, as everywhere else in the clients. */
    const val DEFAULT_PORT = 9777

    /**
     * Parse a `punktfunk://` (or `pf://`) URL. Everything hostile is rejected here, once: over-long
     * input, malformed escapes, control characters, out-of-charset launch ids and fingerprints that
     * aren't fingerprints. What the caller still has to do is *resolve* — the references may name
     * things that don't exist on this device (see [resolveHost]).
     */
    fun parse(url: String): DeepLinkResult {
        if (url.length > MAX_URL_LEN) return DeepLinkResult.Refused(LinkError.TOO_LONG)
        val sep = url.indexOf("://")
        if (sep < 0) return DeepLinkResult.Refused(LinkError.NOT_OUR_SCHEME)
        val scheme = url.substring(0, sep)
        if (!scheme.equals("punktfunk", ignoreCase = true) && !scheme.equals("pf", ignoreCase = true)) {
            return DeepLinkResult.Refused(LinkError.NOT_OUR_SCHEME)
        }
        // A fragment is never part of this grammar; drop it rather than folding it into the last
        // parameter, where it would smuggle unvalidated text past the caps.
        val rest = url.substring(sep + 3).substringBefore('#')
        val path = rest.substringBefore('?').trimEnd('/')
        val query = if (rest.contains('?')) rest.substringAfter('?') else ""

        val slash = path.indexOf('/')
        val routeWord: String
        val hostRefRaw: String
        when {
            slash >= 0 -> {
                routeWord = path.substring(0, slash)
                hostRefRaw = path.substring(slash + 1)
            }
            // A single segment: Apple's shipped links are always `connect/<uuid>`, but a bare
            // reference is unambiguous as long as it isn't one of the route words — those stay
            // routes (with a missing reference), so `punktfunk://pair` refuses instead of hunting
            // for a host called "pair".
            isRouteWord(path) -> {
                routeWord = path
                hostRefRaw = ""
            }
            else -> {
                routeWord = "connect"
                hostRefRaw = path
            }
        }
        val route = when (routeWord.lowercase()) {
            "connect" -> LinkRoute.CONNECT
            "wake" -> LinkRoute.WAKE
            "browse" -> LinkRoute.BROWSE
            "pair" -> return DeepLinkResult.Refused(LinkError.PAIR_REFUSED)
            else -> return DeepLinkResult.Refused(LinkError.UNKNOWN_ROUTE, routeWord)
        }

        val hostRef = when (val d = decode(hostRefRaw)) {
            is Decoded.Err -> return DeepLinkResult.Refused(d.error)
            is Decoded.Ok -> d.text
        }
        if (hostRef.isEmpty()) return DeepLinkResult.Refused(LinkError.MISSING_HOST_REF)
        if (scalarCount(hostRef) > MAX_HOST_REF_LEN) {
            return DeepLinkResult.Refused(LinkError.PARAM_TOO_LONG, "host-ref")
        }

        var fp: String? = null
        var host: Pair<String, Int>? = null
        var launch: String? = null
        var preset: String? = null
        var legacyPreset: String? = null
        var name: String? = null
        for (pair in query.split('&')) {
            if (pair.isEmpty()) continue
            val eq = pair.indexOf('=')
            val rawKey = if (eq >= 0) pair.substring(0, eq) else pair
            val rawValue = if (eq >= 0) pair.substring(eq + 1) else ""
            val key = when (val d = decode(rawKey)) {
                is Decoded.Err -> return DeepLinkResult.Refused(d.error)
                is Decoded.Ok -> d.text.lowercase()
            }
            val value = when (val d = decode(rawValue)) {
                is Decoded.Err -> return DeepLinkResult.Refused(d.error)
                is Decoded.Ok -> d.text
            }
            // `?launch=` with nothing after it is "not given", not an error.
            if (value.isEmpty()) continue
            // First occurrence wins, and unknown keys are ignored: a newer emitter's parameter must
            // not turn an otherwise valid link into a refusal, and appending a second `fp=` must
            // not be able to override the first.
            when {
                key == "fp" && fp == null -> {
                    val hex = value.lowercase()
                    if (hex.length != 64 || !hex.all { it in '0'..'9' || it in 'a'..'f' }) {
                        return DeepLinkResult.Refused(LinkError.BAD_FINGERPRINT)
                    }
                    fp = hex
                }
                key == "host" && host == null ->
                    host = parseAddrPort(value) ?: return DeepLinkResult.Refused(LinkError.BAD_HOST_PARAM)
                key == "launch" && launch == null -> {
                    if (value.toByteArray(StandardCharsets.UTF_8).size > MAX_LAUNCH_LEN) {
                        return DeepLinkResult.Refused(LinkError.PARAM_TOO_LONG, "launch")
                    }
                    if (!isSafeLaunchId(value)) return DeepLinkResult.Refused(LinkError.BAD_LAUNCH_ID)
                    launch = value
                }
                key == "preset" && preset == null -> {
                    if (scalarCount(value) > MAX_PRESET_LEN) {
                        return DeepLinkResult.Refused(LinkError.PARAM_TOO_LONG, "preset")
                    }
                    preset = value
                }
                // The pre-rename spelling: `preset` wins wherever it sits.
                key == "profile" && legacyPreset == null -> {
                    if (scalarCount(value) > MAX_PRESET_LEN) {
                        return DeepLinkResult.Refused(LinkError.PARAM_TOO_LONG, "profile")
                    }
                    legacyPreset = value
                }
                key == "name" && name == null -> {
                    if (scalarCount(value) > MAX_NAME_LEN) {
                        return DeepLinkResult.Refused(LinkError.PARAM_TOO_LONG, "name")
                    }
                    name = value
                }
            }
        }
        return DeepLinkResult.Parsed(DeepLink(route, hostRef, fp, host, launch, preset ?: legacyPreset, name))
    }

    /**
     * Resolve a link's host reference against the local store, in the documented order: stable
     * record id → unique case-insensitive name → `addr[:port]` literal, then the `host=` recovery
     * parameter — a self-emitted shortcut that outlived the record it was written from still lands
     * on the right box.
     *
     * Only the record id is UNGUESSABLE, so only the record id resolves to [HostResolution.Known],
     * the silent one-click contract. A display name comes from an mDNS instance name or a user
     * label ("Gaming PC"), and an address is a LAN address: any zero-permission app or web page can
     * emit `punktfunk://connect/Gaming%20PC` and would otherwise start a stream (and launch a
     * title) on a guess. Those all resolve to [HostResolution.Confirm] — the same host, behind the
     * user's OK.
     */
    fun resolveHost(link: DeepLink, hosts: List<KnownHost>): HostResolution {
        hosts.firstOrNull { it.id == link.hostRef }?.let { return HostResolution.Known(it) }
        val byName = hosts.filter { it.name.equals(link.hostRef, ignoreCase = true) }
        when (byName.size) {
            1 -> return HostResolution.Confirm(byName[0])
            0 -> Unit
            else -> return HostResolution.Ambiguous
        }
        // `addr[:port]` literal, then the `host=` recovery parameter. At that address the link's `fp`
        // picks its record — a dual-boot box answers there with two pins — else only a placeholder
        // stands in. The literal is only considered when the reference COULD be an address: a stale
        // record id must fall through to `host=` (or to a refusal), never be offered as a box to dial.
        val literal = if (looksLikeAddress(link.hostRef)) parseAddrPort(link.hostRef) else null
        for ((addr, port) in listOfNotNull(literal, link.host)) {
            val at = hosts.filter { it.address == addr && it.port == port }
            val match = if (link.fp == null) {
                at.firstOrNull()
            } else {
                at.firstOrNull { it.fpHex.equals(link.fp, ignoreCase = true) }
                    ?: at.firstOrNull { it.fpHex.isEmpty() }
            }
            match?.let { return HostResolution.Confirm(it) }
        }
        val fallback = literal ?: link.host ?: return HostResolution.Unresolvable
        return HostResolution.Unknown(fallback.first, fallback.second, link.name, link.fp)
    }

    /**
     * The self-emitted form for a saved host: id first (address-independent), with the address and
     * pin alongside so the link degrades to a confirmation sheet instead of a dead click when the
     * record is gone.
     */
    fun forHost(host: KnownHost, launch: String? = null, preset: String? = null) = DeepLink(
        route = LinkRoute.CONNECT,
        hostRef = host.id,
        fp = host.fpHex.ifEmpty { null },
        host = host.address to host.port,
        launch = launch,
        preset = preset,
    )

    /** The reserved first path segments — plus `pair`, reserved precisely so it can be refused. */
    private fun isRouteWord(s: String) = s.lowercase() in setOf("connect", "wake", "browse", "pair")

    /**
     * Could this reference be a network address (an IP literal or a host name) rather than a record
     * id or a display name? Only then may an unmatched reference become "an unknown host at this
     * address". A stale record id is NOT an address: offering to dial a UUID as a hostname would
     * turn a wiped store into a confusing dead end instead of the `host=`-driven recovery.
     */
    private fun looksLikeAddress(s: String): Boolean {
        val uuidShaped = s.length == 36 && s.withIndex().all { (i, c) ->
            if (i in setOf(8, 13, 18, 23)) c == '-' else isHex(c)
        }
        return !uuidShaped && s.isNotEmpty() &&
            s.all { it.isLetterOrDigit() && it.code < 128 || it in ".-_:[]" }
    }

    /**
     * `addr`, `addr:port`, `[v6]`, `[v6]:port` — null when the port isn't a number. A bare IPv6
     * literal (`::1`) keeps its colons and takes the default port; anything else splits at the last
     * colon, like every other host-parsing site in the clients.
     */
    private fun parseAddrPort(s: String): Pair<String, Int>? {
        if (s.isEmpty()) return null
        if (s.startsWith("[")) {
            val close = s.indexOf(']', 1)
            if (close < 0) return null
            val addr = s.substring(1, close)
            if (addr.isEmpty()) return null
            val tail = s.substring(close + 1)
            if (tail.isEmpty()) return addr to DEFAULT_PORT
            if (!tail.startsWith(":")) return null
            return addr to (tail.substring(1).toIntOrNull()?.takeIf { it in 1..65535 } ?: return null)
        }
        val lastColon = s.lastIndexOf(':')
        if (lastColon < 0) return s to DEFAULT_PORT
        val head = s.substring(0, lastColon)
        // `::1` and friends: the head still has a colon, so this isn't a port separator.
        if (head.contains(':')) return s to DEFAULT_PORT
        if (head.isEmpty()) return null
        val port = s.substring(lastColon + 1).toIntOrNull()?.takeIf { it in 1..65535 } ?: return null
        return head to port
    }

    /**
     * The launch-id charset the whole product already agrees on: printable, non-space ASCII with no
     * shell metacharacters (Decky rides ids through Steam launch options as an env token, so a
     * quote or a backtick genuinely breaks something downstream). Validation only — the id is
     * opaque and the host matches it verbatim against its own library. No leading `-`: the id
     * rides session argv, where `--browse` would read as a flag.
     */
    private fun isSafeLaunchId(id: String): Boolean =
        id.isNotEmpty() && !id.startsWith('-') && id.toByteArray(StandardCharsets.UTF_8)
            .all { it in 0x21..0x7e && it.toInt().toChar() !in "\"'\\$`" }

    /**
     * Strict percent-decoding: `%` must be followed by exactly two hex digits, the result must be
     * UTF-8, and no control character may survive. Lenient decoders are how `%00`, a stray newline
     * or a half-escape end up inside a filename or a log line.
     */
    private fun decode(s: String): Decoded {
        val bytes = s.toByteArray(StandardCharsets.UTF_8)
        val out = java.io.ByteArrayOutputStream(bytes.size)
        var i = 0
        while (i < bytes.size) {
            val b = bytes[i]
            if (b == '%'.code.toByte()) {
                if (i + 2 >= bytes.size) return Decoded.Err(LinkError.BAD_ESCAPE)
                val hi = hexValue(bytes[i + 1].toInt().toChar())
                val lo = hexValue(bytes[i + 2].toInt().toChar())
                if (hi < 0 || lo < 0) return Decoded.Err(LinkError.BAD_ESCAPE)
                out.write(hi * 16 + lo)
                i += 3
            } else {
                out.write(b.toInt())
                i += 1
            }
        }
        // REPORT, not the default REPLACE: `%FF` must be a refusal, never a U+FFFD that survives.
        val decoder = StandardCharsets.UTF_8.newDecoder()
            .onMalformedInput(CodingErrorAction.REPORT)
            .onUnmappableCharacter(CodingErrorAction.REPORT)
        val text = runCatching { decoder.decode(ByteBuffer.wrap(out.toByteArray())).toString() }
            .getOrNull() ?: return Decoded.Err(LinkError.BAD_ESCAPE)
        // `Char.isISOControl` is exactly Unicode's Cc category (C0 + DEL + C1), which is what the
        // Rust side rejects.
        if (text.any { it.isISOControl() }) return Decoded.Err(LinkError.CONTROL_CHAR)
        return Decoded.Ok(text)
    }

    /** [decode]'s outcome — the decoded text, or which refusal it is. */
    private sealed interface Decoded {
        data class Ok(val text: String) : Decoded
        data class Err(val error: LinkError) : Decoded
    }

    private fun hexValue(c: Char): Int = when (c) {
        in '0'..'9' -> c - '0'
        in 'a'..'f' -> c - 'a' + 10
        in 'A'..'F' -> c - 'A' + 10
        else -> -1
    }

    private fun isHex(c: Char) = hexValue(c) >= 0

    /** Unicode scalars, matching the Rust caps (which count `chars()`, not UTF-16 units). */
    private fun scalarCount(s: String) = s.codePointCount(0, s.length)

    /**
     * Percent-encode for emission: unreserved characters plus `:` (legal in a query value and left
     * alone by Apple's `URLComponents`, so the three emitters agree on `steam:570`).
     */
    internal fun encode(s: String): String {
        val out = StringBuilder(s.length)
        for (b in s.toByteArray(StandardCharsets.UTF_8)) {
            val c = b.toInt().toChar()
            if (c in 'A'..'Z' || c in 'a'..'z' || c in '0'..'9' || c in "-._~:") {
                out.append(c)
            } else {
                out.append('%').append("%02X".format(b.toInt() and 0xFF))
            }
        }
        return out.toString()
    }
}

/** What the URL asks for. `WAKE`/`BROWSE` are reserved in the grammar and parse today. */
enum class LinkRoute(val word: String) {
    CONNECT("connect"),
    WAKE("wake"),
    BROWSE("browse"),
}

/**
 * Why a URL was rejected. The [code] strings are the cross-language contract — the vector file names
 * them, and Swift and Rust report the same code for the same input.
 */
enum class LinkError(val code: String) {
    /** Not a `punktfunk://` (or `pf://`) URL at all — ignore it, don't warn. */
    NOT_OUR_SCHEME("not-our-scheme"),
    TOO_LONG("too-long"),
    UNKNOWN_ROUTE("unknown-route"),

    /** `punktfunk://pair/…` — pairing is an interactive ceremony, never a link. */
    PAIR_REFUSED("pair-refused"),
    MISSING_HOST_REF("missing-host-ref"),

    /** A `%` escape that isn't two hex digits, or a decode that isn't UTF-8. */
    BAD_ESCAPE("bad-escape"),

    /** A control character survived decoding — no legitimate field contains one. */
    CONTROL_CHAR("control-char"),
    PARAM_TOO_LONG("param-too-long"),
    BAD_FINGERPRINT("bad-fingerprint"),
    BAD_HOST_PARAM("bad-host-param"),
    BAD_LAUNCH_ID("bad-launch-id"),
}

/**
 * A parsed, validated link. Every field is already length- and charset-checked, so a consumer never
 * has to re-validate hostile input.
 */
data class DeepLink(
    val route: LinkRoute = LinkRoute.CONNECT,
    /** The host reference as written: a stable record id, a host name, or `addr[:port]`. */
    val hostRef: String,
    /** Expected host certificate fingerprint, lowercase hex (64 chars). */
    val fp: String? = null,
    /** Recovery address for a stable id that no longer resolves (store wiped, reinstall). */
    val host: Pair<String, Int>? = null,
    /** A store-qualified library id (`steam:570`) for the host to launch on arrival. */
    val launch: String? = null,
    /** A settings-preset reference (id, or a unique name) — one-off, never rebinding. */
    val preset: String? = null,
    /** Display label for the unknown-host confirmation sheet (external emitters). */
    val name: String? = null,
) {
    /** The canonical URL for this link — always `punktfunk://`, never the `pf://` alias. */
    fun toUrl(): String {
        val sb = StringBuilder("punktfunk://${route.word}/${DeepLinks.encode(hostRef)}")
        var sep = '?'
        fun push(key: String, value: String) {
            sb.append(sep).append(key).append('=').append(DeepLinks.encode(value))
            sep = '&'
        }
        fp?.let { push("fp", it) }
        host?.let { (addr, port) ->
            push(
                "host",
                when {
                    port == DeepLinks.DEFAULT_PORT -> addr
                    addr.contains(':') -> "[$addr]:$port" // literal IPv6 needs its brackets back
                    else -> "$addr:$port"
                },
            )
        }
        launch?.let { push("launch", it) }
        // `profile=` as well, for clients that predate `preset=`.
        preset?.let {
            push("preset", it)
            push("profile", it)
        }
        name?.let { push("name", it) }
        return sb.toString()
    }

    /**
     * True when this link's `fp` contradicts what we have pinned for that host — the link is stale
     * or lying, and the only safe answer is a hard refusal.
     */
    fun pinConflict(host: KnownHost): Boolean =
        fp != null && host.fpHex.isNotEmpty() && !fp.equals(host.fpHex, ignoreCase = true)
}

/** A parse outcome: a link, or a refusal carrying the shared code. */
sealed interface DeepLinkResult {
    data class Parsed(val link: DeepLink) : DeepLinkResult

    data class Refused(val error: LinkError, val detail: String? = null) : DeepLinkResult {
        /**
         * A sentence for the notice a refusing front-end shows. Deliberately names what failed: a
         * shortcut that can't honour its reference says so instead of streaming with the wrong one.
         */
        fun message(): String = when (error) {
            LinkError.NOT_OUR_SCHEME -> "That isn't a Punktfunk link."
            LinkError.TOO_LONG -> "That link is too long to be genuine."
            LinkError.UNKNOWN_ROUTE -> "Punktfunk links can't do “$detail”."
            LinkError.PAIR_REFUSED ->
                "Pairing can't be done from a link — pair the host in Punktfunk first."
            LinkError.MISSING_HOST_REF -> "That link doesn't say which host to use."
            LinkError.BAD_ESCAPE, LinkError.CONTROL_CHAR -> "That link is malformed and was ignored."
            LinkError.PARAM_TOO_LONG -> "That link's “$detail” value is too long."
            LinkError.BAD_FINGERPRINT -> "That link's host fingerprint isn't a valid one."
            LinkError.BAD_HOST_PARAM -> "That link's host address isn't valid."
            LinkError.BAD_LAUNCH_ID -> "That link's game id isn't a valid one."
        }
    }
}

/** What the local host store made of a link's references. */
sealed interface HostResolution {
    /** A saved record — [Known] may act on its own, [Confirm] only once the user says so. */
    sealed interface Record : HostResolution {
        val host: KnownHost
    }

    /**
     * A record we already trust, named by its stable (unguessable) id: the one-click contract,
     * subject to [DeepLink.pinConflict].
     */
    data class Known(override val host: KnownHost) : Record

    /**
     * The same record, but named by something GUESSABLE — its display name, its address, or the
     * `host=` recovery parameter. A link may not start a stream on a guess, so this one goes to
     * the confirmation the front-end shows: same dial, one tap later.
     */
    data class Confirm(override val host: KnownHost) : Record

    /**
     * No record, but the link says where to dial: the confirmation sheet's input, from which the
     * normal pairing/TOFU flow proceeds under the user's eyes. Never an auto-connect.
     */
    data class Unknown(
        val address: String,
        val port: Int,
        val name: String?,
        val fp: String?,
    ) : HostResolution

    /** The name matched more than one saved host — refuse with a notice, never guess. */
    data object Ambiguous : HostResolution

    /** A reference that resolves to nothing and carries no address to fall back on. */
    data object Unresolvable : HostResolution
}
