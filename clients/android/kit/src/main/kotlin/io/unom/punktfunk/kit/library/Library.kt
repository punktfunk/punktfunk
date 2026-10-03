package io.unom.punktfunk.kit.library

import android.util.Log
import io.unom.punktfunk.kit.NativeBridge
import okhttp3.Cache
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.io.ByteArrayInputStream
import java.security.KeyFactory
import java.security.KeyStore
import java.security.MessageDigest
import java.security.PrivateKey
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import java.security.spec.PKCS8EncodedKeySpec
import java.util.Base64
import java.util.concurrent.TimeUnit
import javax.net.ssl.HostnameVerifier
import javax.net.ssl.HttpsURLConnection
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.TrustManager
import javax.net.ssl.TrustManagerFactory
import javax.net.ssl.X509TrustManager

// Android game-library client — the mirror of the Apple client's LibraryClient.swift. Fetches a
// host's unified game library from its management REST API (`GET /api/v1/library`) over **mTLS**: the
// paired client presents its persistent cert/key (the same identity the host paired over QUIC), and
// the host's self-signed cert is pinned by SHA-256(DER). Reads the library and what is running;
// [LibraryClient.endGame] is the one write. Mirrors the GameEntry/Artwork schema in
// crates/punktfunk-host/src/library.rs.

/** The management API's default port — matches `mgmt::DEFAULT_PORT` on the host and the Apple client. */
const val DEFAULT_MGMT_PORT = 47990

/**
 * `https://<address>:<port>` for the management API. An IPv6 literal goes in brackets, whether it
 * was saved bare or bracketed — the desktop's `base_url` and Apple's `baseURL`.
 */
fun mgmtBase(address: String, port: Int): String {
    val bare = address.removeSurrounding("[", "]")
    return if (':' in bare) "https://[$bare]:$port" else "https://$bare:$port"
}

/** Cover-art URLs. Steam art arrives as host-relative proxy paths, resolved to absolute by [LibraryClient]. */
data class Artwork(val portrait: String?, val header: String?, val hero: String?) {
    /** Poster preference for a 2:3 tile: portrait capsule → header → hero (near-universal fallbacks). */
    val posterCandidates: List<String> get() = listOfNotNull(portrait, header, hero)
}

/** A title's play numbers as the host keeps them (`GameEntry.stats`), in the host's keys. */
data class GameStats(
    val lastPlayedUnixMs: Long = 0,
    val playTimeMs: Long = 0,
    val lastRunMs: Long = 0,
    val launchCount: Int = 0,
) {
    fun toJson(): JSONObject = JSONObject()
        .put("last_played_unix_ms", lastPlayedUnixMs)
        .put("play_time_ms", playTimeMs)
        .put("last_run_ms", lastRunMs)
        .put("launch_count", launchCount)

    companion object {
        /** A missing number reads as zero: numbers are never worth an empty library. */
        fun from(o: JSONObject?): GameStats? = o?.let {
            GameStats(
                lastPlayedUnixMs = it.optLong("last_played_unix_ms"),
                playTimeMs = it.optLong("play_time_ms"),
                lastRunMs = it.optLong("last_run_ms"),
                launchCount = it.optInt("launch_count"),
            )
        }
    }
}

/**
 * One title in the unified library. [id] is store-qualified (`steam:<appid>` / `custom:<id>`).
 *
 * [role] is `"game"` (the default, and what an older host omits) or `"launcher"` — an entry that
 * opens the launcher itself (Steam Big Picture, Heroic) rather than a title. Kept a plain nullable
 * String on purpose: the host owns the vocabulary, and an unknown future value must degrade to a
 * game rather than break the decode (design D4).
 *
 * [icon] is the token for the entry's brand mark (`"steam"`, `"heroic"`) — never art, never a URL.
 * Null on every older host and on every ordinary title.
 */
data class GameEntry(
    val id: String,
    val store: String,
    val title: String,
    val art: Artwork,
    val role: String? = null,
    val icon: String? = null,
    /**
     * The host's platform tag (`platform` in the catalog — a ROM manager's console name; Steam
     * sets none). What the console's Collections group by; carried through verbatim.
     */
    val platform: String? = null,
    /**
     * The rest of the host's `GameMeta` that a screen has room for. The host has sent these since
     * the library API existed and nothing decoded them until the launch hold wanted more than a
     * title; every one is absent on an older host, which simply shows less.
     */
    val developer: String? = null,
    val releaseYear: Int? = null,
    val genres: List<String> = emptyList(),
    /** Null until the host has launched the title once. */
    val stats: GameStats? = null,
) {
    val isCustom: Boolean get() = store == "custom"

    /** Whether this entry opens a launcher rather than a game. */
    val isLauncher: Boolean get() = role == "launcher"

    /** The synthetic desktop tile rather than one of the host's titles. */
    val isDesktop: Boolean get() = id == DESKTOP_ID

    companion object {
        /**
         * The desktop tile's id — the synthetic entry every shelf leads with, so the library is
         * never a dead end for the desktop-only user and a host with no plugins is still one tap
         * from streaming. The NUL prefix is the desktop console's (`pf-console-ui`'s
         * `DESKTOP_ID`): a host title id is a store reference, and none can start with a NUL.
         *
         * Presentation only. Never persisted, never fetched, never grouped.
         */
        const val DESKTOP_ID = "\u0000desktop"

        /** The tile itself. [title] reads "Resume …" when the host already has something up. */
        fun desktop(title: String = "Desktop") = GameEntry(
            id = DESKTOP_ID,
            store = "",
            title = title,
            art = Artwork(portrait = null, header = null, hero = null),
        )
    }

    /**
     * The brand-icon token, re-validated rather than taken on trust.
     *
     * The host checks the shape on the way in, so this only fires for a host older than that
     * check or one that isn't ours. It costs a scan of a short string and means no consumer has
     * to wonder what it is about to look up.
     */
    val iconToken: String? get() = icon?.takeIf { t ->
        t.isNotEmpty() && t.length <= 32 && t[0] in 'a'..'z' &&
            t.all { it in 'a'..'z' || it in '0'..'9' || it == '-' }
    }

    /**
     * Display name for the store badge — the same table the other clients use
     * (`pf-console-ui::library::store_label`). Before this the UI said "Steam" for every non-custom
     * entry, which a Lutris or GOG title made a lie.
     */
    val storeLabel: String get() = when (store) {
        "steam" -> "Steam"
        "custom" -> "Custom"
        "heroic" -> "Heroic"
        "lutris" -> "Lutris"
        "epic" -> "Epic"
        "gog" -> "GOG"
        "xbox" -> "Xbox"
        else -> "Game"
    }
}

/**
 * Design D4: launcher entries lead the shelf, keeping the host's title order within each group.
 * Applied once where the library is fetched, so no screen has to remember the rule — and a library
 * without launcher entries comes back untouched.
 */
fun List<GameEntry>.launchersFirst(): List<GameEntry> {
    val launchers = filter { it.isLauncher }
    return if (launchers.isEmpty()) this else launchers + filterNot { it.isLauncher }
}

/** Fetch outcome — three states so the UI can guide setup (the common case is "not paired yet"). */
sealed class LibraryResult {
    data class Ok(val games: List<GameEntry>) : LibraryResult()
    data class Unauthorized(val message: String) : LibraryResult()
    data class Error(val message: String) : LibraryResult()

    /**
     * Is this the "can't reach it" failure — the only one worth waiting out?
     *
     * A rejected certificate does not become acceptable by retrying, and asking an unpaired host
     * twelve times only delays telling the user what is actually wrong. Lives here rather than at
     * the call site so the retry loop and the error copy can never disagree about which failures
     * are transient.
     */
    val isTransient: Boolean get() = this is Error
}

/** `GET /api/v1/status`, the slice a client reads. */
data class HostStatus(
    val games: List<RunningGame> = emptyList(),
    val downloads: List<Download> = emptyList(),
)

/**
 * One title's download, from `GET /api/v1/status` `downloads[]`. Its words match the Rust console
 * shell's (`pf_client_core::library::DownloadProgress`), so a report quotes one line either way.
 */
data class Download(
    val appId: String,
    /** `queued` | `downloading` | `paused` | `installing` | `done` | `failed` | `cancelled`. */
    val state: String,
    val doneBytes: Long = 0,
    val totalBytes: Long? = null,
    val rateBps: Long? = null,
    val etaS: Long? = null,
    val phase: String? = null,
    val error: String? = null,
) {
    /** Making progress, or expected to: a launch waits on it. */
    val live: Boolean get() = state == "queued" || state == "downloading" || state == "installing"

    /** 0–1 of the total, when the total is known. */
    val fraction: Float?
        get() = totalBytes?.takeIf { it > 0 }?.let { (doneBytes.toFloat() / it).coerceIn(0f, 1f) }

    /** `12.3 GB of 26 GB · 48 MB/s · about 4 min left`, `Installing…`. */
    fun line(): String {
        when (state) {
            "queued" -> return "Waiting for its turn to download…"
            "installing" -> return phase ?: "Installing…"
        }
        val total = totalBytes?.takeIf { it > 0 }
        val parts = mutableListOf(
            if (total != null) "${humanBytes(doneBytes)} of ${humanBytes(total)}"
            else "${humanBytes(doneBytes)} so far",
        )
        if (state == "downloading") {
            rateBps?.let { parts.add("${humanBytes(it)}/s") }
            etaS?.let {
                parts.add(
                    when {
                        it < 60 -> "less than a minute left"
                        it < 3600 -> "about ${(it + 30) / 60} min left"
                        else -> "about ${it / 3600} h ${(it % 3600) / 60} min left"
                    },
                )
            }
        }
        return parts.joinToString(" · ")
    }

    /** Why a launch that waited on it didn't start the title; null while it may still. */
    fun stopped(title: String): String? = when (state) {
        "failed" -> error?.let { "$title didn't download — $it" } ?: "$title didn't download."
        "cancelled" -> "$title's download was cancelled."
        "paused" -> "$title's download was paused. Start it again to resume."
        else -> null
    }
}

/** Decimal units, as stores count: `12.3 GB`, `48 MB`, `512 kB`. */
fun humanBytes(n: Long): String {
    val (value, unit) = when {
        n >= 1_000_000_000L -> n / 1e9 to "GB"
        n >= 1_000_000L -> n / 1e6 to "MB"
        else -> n / 1e3 to "kB"
    }
    val rounded = Math.round(value * 10) / 10.0
    return if (value >= 100 || rounded % 1.0 == 0.0) "${Math.round(value)} $unit"
    else "${String.format(java.util.Locale.ROOT, "%.1f", rounded)} $unit"
}

/**
 * One game the host currently has launched, from `GET /api/v1/status`.
 *
 * A deliberately partial mirror of the host's `ActiveGame`: only the fields a client can act on.
 * The web console's view of this payload carries more (which session, which plane, the grace
 * countdown), and none of that is a player's business from the library shelf.
 */
data class RunningGame(
    /**
     * Store-qualified library id (`steam:570`) — the key that lines this up with a [GameEntry].
     * Null for an operator-typed GameStream command, which has no catalog entry behind it.
     */
    val appId: String?,
    val title: String,
    /**
     * `launching` | `running` | `window` | `exited` | `untracked` | `grace` | `detached`. A plain
     * String on purpose: the host owns the vocabulary and adds to it, so an unknown value must
     * never fail the decode of the whole list.
     */
    val state: String,
    /** `running`, and the host will report `window` once the game's window is up. */
    val awaitingWindow: Boolean = false,
    /** The live session streaming it; null for a game nobody streams. */
    val sessionId: Long? = null,
    /** This device may end it ([LibraryClient.endGame]): a game it launched. False from an older host. */
    val endable: Boolean = false,
) {
    /** A game this device launched that a live session streams: what an in-stream End game ends. */
    val streamedHere: Boolean get() = endable && sessionId != null && appId != null

    /**
     * Is this title *up on the host right now* — i.e. would picking it take the player back into
     * it rather than start it?
     *
     * `untracked` counts: the host cannot follow that process, but it did launch it and has no
     * evidence it stopped. `grace` counts too — its session is gone but the game is still running,
     * which is precisely the case where getting back in promptly matters most. Only a confirmed
     * `exited` does not.
     */
    val isUp: Boolean get() = state != "exited"
}

/** What asking the host to end a game came to (`POST /api/v1/game/end`). The Rust client's `GameEnd`. */
sealed class GameEnd {
    data object Ended : GameEnd()
    /** 409: the host had nothing of this title left to end. */
    data object NotRunning : GameEnd()
    /** 401/404: a host that predates ending games from a device. */
    data object Unsupported : GameEnd()
    /** 403: this device's access to the host expired. */
    data object Expired : GameEnd()
    data class Failed(val why: String) : GameEnd()

    /** The game is gone, so a stream that was playing it can end. */
    val gameGone: Boolean get() = this == Ended || this == NotRunning

    /** The player-facing line. The Rust, Swift and web clients use the same words. */
    fun notice(title: String): String = when (this) {
        Ended -> "Ended $title."
        NotRunning -> "$title isn't running any more."
        Unsupported -> "This host needs an update to end games from here."
        Expired -> "This device's access to the host has expired."
        is Failed -> "Couldn't end $title \u2014 $why"
    }

    companion object {
        fun fromStatus(code: Int): GameEnd = when (code) {
            in 200..299 -> Ended
            409 -> NotRunning
            401, 404 -> Unsupported
            403 -> Expired
            else -> Failed("the host refused it ($code)")
        }
    }
}

object LibraryClient {
    private const val TAG = "LibraryClient"

    /** Titles a request: the host's ceiling for one page. */
    internal const val PAGE_LIMIT = 200

    /** 500 pages of 200 is 100 000 titles. A host whose cursor never runs out stops here. */
    internal const val MAX_PAGES = 500

    /** What walking the pages came to: the catalog, or the status that stopped it. */
    internal sealed class Walk {
        data class Done(val games: List<GameEntry>) : Walk()
        data class Refused(val code: Int) : Walk()
    }

    /** The request path of one page. The cursor is the host's own text, so it is encoded. */
    internal fun pagePath(cursor: String?): String =
        "/api/v1/library/page?limit=$PAGE_LIMIT" +
            (cursor?.let { "&cursor=" + java.net.URLEncoder.encode(it, "UTF-8") } ?: "")

    /**
     * The whole catalog, a page at a time, so no answer grows with the library. [get] takes the
     * cursor of the page before and answers one page's status and body. Any page failing fails
     * the walk: half a catalog is not one.
     */
    internal fun walkPages(base: String, get: (cursor: String?) -> Pair<Int, String>): Walk {
        val games = ArrayList<GameEntry>()
        var cursor: String? = null
        repeat(MAX_PAGES) {
            val (code, body) = get(cursor)
            if (code != 200) return Walk.Refused(code)
            val page = JSONObject(body)
            games += parseItems(page.getJSONArray("items"), base)
            val next = str(page, "next_cursor")
            // A cursor that does not move would ask for the same page forever.
            if (next == null || next == cursor) return Walk.Done(games)
            cursor = next
        }
        return Walk.Done(games)
    }

    /**
     * The host's catalog, walked by `GET /api/v1/library/page` at [mgmtBase] and authenticated
     * by mTLS. A host older than the paged route refuses it on this lane, so `GET
     * /api/v1/library` answers whole instead. [fpHex] is the pinned host-cert SHA-256 (64 hex,
     * from the paired [io.unom.punktfunk.kit.security.KnownHost]); a blank value means the host
     * was never paired. A refusal maps through [refused]. BLOCKING — call from a background
     * dispatcher.
     */
    fun fetch(
        address: String,
        mgmtPort: Int = DEFAULT_MGMT_PORT,
        certPem: String,
        keyPem: String,
        fpHex: String,
    ): LibraryResult {
        if (fpHex.isBlank()) {
            return LibraryResult.Unauthorized(
                "connect to this host once first — pairing is what lets it show its games",
            )
        }
        val client = try {
            mtlsHttpClient(certPem, keyPem, address, fpHex)
        } catch (e: Exception) {
            Log.w(TAG, "mTLS client for $address", e)
            return LibraryResult.Error("couldn't set up a secure connection to the host")
        }
        val base = mgmtBase(address, mgmtPort)
        val get = { path: String ->
            client.newCall(Request.Builder().url(base + path).build()).execute()
                .use { it.code to it.body?.string().orEmpty() }
        }
        return try {
            when (val walked = walkPages(base) { cursor -> get(pagePath(cursor)) }) {
                is Walk.Done -> LibraryResult.Ok(walked.games.launchersFirst())
                is Walk.Refused -> if (walked.code in setOf(401, 403, 404)) {
                    val (code, body) = get("/api/v1/library")
                    if (code == 200) LibraryResult.Ok(parse(body, base)) else refused(code)
                } else {
                    refused(walked.code)
                }
            }
        } catch (e: Exception) {
            Log.w(TAG, "library fetch from $base", e)
            LibraryResult.Error("couldn't reach the host — check that it's on and on this network")
        }
    }

    /**
     * What the host currently has running, from `GET /api/v1/status`.
     *
     * Same lane, same identity, no new host work: `/status` is already on the paired-certificate
     * allowlist (the host's `mgmt::auth::cert_may_access`) alongside `/library`, and has carried a
     * `games[]` array since the session⇄game lifetime work. This client simply never read it — so
     * a player had no way to see, from the device they browse on, that something was already up.
     *
     * **Best-effort by contract**: an older host, an unreachable one, or a shape we don't recognize
     * yields an empty list rather than an error. Nothing here is worth failing a library screen
     * over — the worst case is a Resume badge that doesn't appear. BLOCKING; call from IO.
     */
    fun fetchRunning(
        address: String,
        mgmtPort: Int = DEFAULT_MGMT_PORT,
        certPem: String,
        keyPem: String,
        fpHex: String,
    ): List<RunningGame> = fetchStatus(address, mgmtPort, certPem, keyPem, fpHex).games

    /**
     * `GET /api/v1/status`: the launched titles and the host's downloads, kept apart because a
     * launch the host declined over its download has no game row left to carry it. Best-effort,
     * as [fetchRunning]. BLOCKING; call from IO.
     */
    fun fetchStatus(
        address: String,
        mgmtPort: Int = DEFAULT_MGMT_PORT,
        certPem: String,
        keyPem: String,
        fpHex: String,
    ): HostStatus {
        if (fpHex.isBlank()) return HostStatus()
        return try {
            val client = mtlsHttpClient(certPem, keyPem, address, fpHex)
            val req = Request.Builder().url("${mgmtBase(address, mgmtPort)}/api/v1/status").build()
            client.newCall(req).execute().use { resp ->
                if (resp.code != 200) return HostStatus()
                parseStatus(resp.body?.string().orEmpty())
            }
        } catch (_: Exception) {
            HostStatus()
        }
    }

    /**
     * `POST /api/v1/game/end` for one title, live session included (`streaming`). The host ends it
     * only if this device launched it. BLOCKING; call from IO.
     */
    fun endGame(
        address: String,
        mgmtPort: Int = DEFAULT_MGMT_PORT,
        certPem: String,
        keyPem: String,
        fpHex: String,
        appId: String,
    ): GameEnd {
        if (fpHex.isBlank() || appId.isBlank()) return GameEnd.Failed("this host isn't paired")
        return try {
            val body = JSONObject().put("app_id", appId).put("streaming", true)
            val req = Request.Builder()
                .url("${mgmtBase(address, mgmtPort)}/api/v1/game/end")
                .post(body.toString().toRequestBody("application/json".toMediaType()))
                .build()
            mtlsHttpClient(certPem, keyPem, address, fpHex).newCall(req).execute()
                .use { GameEnd.fromStatus(it.code) }
        } catch (e: Exception) {
            Log.w(TAG, "end game failed", e)
            GameEnd.Failed(e.message ?: "couldn't reach the host")
        }
    }

    /** A non-200 answer. 401 and 403 both mean "not paired", as on desktop and Apple. */
    internal fun refused(code: Int): LibraryResult =
        if (code == 401 || code == 403) {
            LibraryResult.Unauthorized("the host doesn't recognize this device — pair with it first")
        } else {
            LibraryResult.Error("the host refused it ($code)")
        }

    /** Tries per wake: a cold box takes 20–60 s to serve, and 12 × 5 s covers that. */
    internal const val WAKE_ATTEMPTS = 12
    internal const val WAKE_RETRY_MS = 5_000L

    /** Re-send the magic packet every other attempt: one packet can be missed. */
    internal const val WAKE_RESEND_EVERY = 2

    /**
     * [fetch] across a host's boot window, for both Android shells (the desktop's `spawn_fetch`).
     *
     * With [autoWake] on and a MAC to send to, a magic packet goes out first, [onWaking] runs
     * once, and the fetch is retried [WAKE_ATTEMPTS] times while it stays transient, resending
     * the packet on the way. Otherwise it is one plain fetch and no packet. Null only when
     * [isCancelled] stopped it before an answer. BLOCKING.
     */
    fun fetchAcrossWake(
        address: String,
        mgmtPort: Int,
        certPem: String,
        keyPem: String,
        fpHex: String,
        macs: List<String>,
        autoWake: Boolean,
        isCancelled: () -> Boolean = { false },
        onWaking: () -> Unit = {},
    ): LibraryResult? = acrossWake(
        waking = autoWake && macs.isNotEmpty(),
        fetch = { fetch(address, mgmtPort, certPem, keyPem, fpHex) },
        wake = { NativeBridge.nativeWakeOnLan(macs.joinToString(","), address) },
        isCancelled = isCancelled,
        onWaking = onWaking,
        sleep = { Thread.sleep(it) },
    )

    /** [fetchAcrossWake] with its effects passed in, so the cadence is testable off-device. */
    internal fun acrossWake(
        waking: Boolean,
        fetch: () -> LibraryResult,
        wake: () -> Unit,
        isCancelled: () -> Boolean,
        onWaking: () -> Unit,
        sleep: (Long) -> Unit,
    ): LibraryResult? {
        if (waking) {
            wake()
            onWaking()
        }
        val attempts = if (waking) WAKE_ATTEMPTS else 1
        var last: LibraryResult? = null
        for (attempt in 0 until attempts) {
            if (isCancelled()) break
            val res = fetch()
            last = res
            if (!res.isTransient || attempt + 1 >= attempts) break
            if (attempt % WAKE_RESEND_EVERY == WAKE_RESEND_EVERY - 1) wake()
            sleep(WAKE_RETRY_MS)
        }
        return last
    }

    /** Just the `games[]` slice of `/status`; everything else on that payload is the console's. */
    internal fun parseStatus(json: String): HostStatus {
        val downloads = JSONObject(json).optJSONArray("downloads") ?: JSONArray()
        val out = ArrayList<Download>(downloads.length())
        for (i in 0 until downloads.length()) {
            val o = downloads.optJSONObject(i) ?: continue
            out.add(
                Download(
                    appId = o.optString("app_id"),
                    state = o.optString("state"),
                    doneBytes = o.optLong("done_bytes"),
                    totalBytes = if (o.has("total_bytes")) o.optLong("total_bytes") else null,
                    rateBps = if (o.has("rate_bps")) o.optLong("rate_bps") else null,
                    etaS = if (o.has("eta_s")) o.optLong("eta_s") else null,
                    phase = str(o, "phase"),
                    error = str(o, "error"),
                ),
            )
        }
        return HostStatus(parseRunning(json), out)
    }

    internal fun parseRunning(json: String): List<RunningGame> {
        val arr = JSONObject(json).optJSONArray("games") ?: return emptyList()
        val out = ArrayList<RunningGame>(arr.length())
        for (i in 0 until arr.length()) {
            val o = arr.optJSONObject(i) ?: continue
            out.add(
                RunningGame(
                    appId = str(o, "app_id"),
                    title = o.optString("title"),
                    state = o.optString("state"),
                    awaitingWindow = o.optBoolean("awaiting_window"),
                    sessionId = if (o.isNull("session_id")) null else o.optLong("session_id"),
                    endable = o.optBoolean("endable"),
                ),
            )
        }
        return out
    }

    private fun parse(json: String, base: String): List<GameEntry> =
        parseItems(JSONArray(json), base).launchersFirst()

    /** The titles of one answer, in the host's order. */
    private fun parseItems(arr: JSONArray, base: String): List<GameEntry> {
        val out = ArrayList<GameEntry>(arr.length())
        for (i in 0 until arr.length()) {
            val o = arr.getJSONObject(i)
            val art = o.optJSONObject("art") ?: JSONObject()
            out.add(
                GameEntry(
                    id = o.optString("id"),
                    store = o.optString("store"),
                    title = o.optString("title"),
                    art = Artwork(
                        portrait = resolveArt(str(art, "portrait"), base),
                        header = resolveArt(str(art, "header"), base),
                        hero = resolveArt(str(art, "hero"), base),
                    ),
                    role = str(o, "role"),
                    icon = str(o, "icon"),
                    platform = str(o, "platform"),
                    developer = str(o, "developer"),
                    releaseYear = o.optInt("release_year").takeIf { it > 0 },
                    genres = o.optJSONArray("genres")?.let { g ->
                        (0 until g.length()).mapNotNull { g.optString(it).ifBlank { null } }
                    } ?: emptyList(),
                    stats = GameStats.from(o.optJSONObject("stats")),
                ),
            )
        }
        return out
    }

    /** A present, non-null, non-blank JSON string field, else null. */
    private fun str(o: JSONObject, key: String): String? =
        if (o.has(key) && !o.isNull(key)) o.optString(key).ifBlank { null } else null

    /** Host-relative art path (`/api/v1/library/art/...`) → absolute against the host; else unchanged. */
    private fun resolveArt(s: String?, base: String): String? =
        if (s != null && s.startsWith("/")) base + s else s
}

/**
 * An OkHttpClient that presents the paired client cert and pins the host's self-signed cert by
 * SHA-256(DER) — reused for BOTH the library fetch and the cover-art loads (so a paired client
 * reaches the host's own art proxy). The pinning trust manager trusts the host by fingerprint and
 * defers to normal public trust for any other origin (an external CDN URL).
 *
 * The two checks are only sound TOGETHER: the trust manager cannot fail closed on its own (it has
 * no hostname, so it must let a CDN chain through), so the hostname verifier is what makes [host]
 * pin-only, matched by the name OkHttp gives it ([urlHost]). Loosen either and a publicly-trusted
 * certificate for any name is accepted for the host. The host's own cert is self-signed with no
 * matching SAN, so it can never satisfy the default verifier; the pin is its only credential.
 *
 * [cache]: an HTTP cache the client honours (`Cache-Control` / `ETag`, which the host's art proxy
 * sends). One instance per directory: OkHttp forbids two on the same path.
 *
 * One connection pool per identity, host and pin, shared by every caller: a pool per call leaves
 * each connection idle for five minutes, and the host drops a peer's 33rd connection.
 */
fun mtlsHttpClient(certPem: String, keyPem: String, host: String, fpHex: String, cache: Cache? = null): OkHttpClient {
    val base = mtlsClients.computeIfAbsent("$host|${fpHex.lowercase()}|$certPem") {
        buildMtlsClient(certPem, keyPem, host, fpHex)
    }
    return if (cache == null) base else base.newBuilder().cache(cache).build()
}

private val mtlsClients = java.util.concurrent.ConcurrentHashMap<String, OkHttpClient>()

private fun buildMtlsClient(certPem: String, keyPem: String, host: String, fpHex: String): OkHttpClient {
    val pinnedHost = urlHost(host)
    val clientCert = CertificateFactory.getInstance("X.509")
        .generateCertificate(ByteArrayInputStream(certPem.toByteArray())) as X509Certificate
    val privateKey = parsePrivateKey(keyPem)

    val keyStore = KeyStore.getInstance("PKCS12").apply {
        load(null, null)
        setKeyEntry("client", privateKey, CharArray(0), arrayOf(clientCert))
    }
    val kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm())
    kmf.init(keyStore, CharArray(0))

    // System default trust manager, for non-host (external CDN) origins.
    val sysTmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm())
    sysTmf.init(null as KeyStore?)
    val sysTm = sysTmf.trustManagers.filterIsInstance<X509TrustManager>().first()

    val pinned = fpHex.lowercase()
    val trustManager = object : X509TrustManager {
        override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) {}
        override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
            if (sha256Hex(chain[0].encoded) == pinned) return // the pinned host
            sysTm.checkServerTrusted(chain, authType) // external CDN — normal public trust
        }
        override fun getAcceptedIssuers(): Array<X509Certificate> = sysTm.acceptedIssuers
    }

    val ssl = SSLContext.getInstance("TLS")
    ssl.init(kmf.keyManagers, arrayOf<TrustManager>(trustManager), null)

    val defaultVerifier = HttpsURLConnection.getDefaultHostnameVerifier()
    val verifier = HostnameVerifier { hostname, session ->
        if (hostname == pinnedHost) {
            // The PINNED host fails closed: only the pinned leaf is acceptable for this name. The
            // trust manager lets any public chain through, so without this a CA-issued cert for
            // any name would stand in for the host and receive the client's mTLS identity.
            try {
                sha256Hex((session.peerCertificates.firstOrNull() as? X509Certificate)?.encoded ?: return@HostnameVerifier false) == pinned
            } catch (_: Exception) {
                false
            }
        } else {
            // Any other origin (an external CDN art URL) is ordinary public trust: the system
            // trust manager validated the chain, and this checks the name against it.
            defaultVerifier.verify(hostname, session)
        }
    }

    return OkHttpClient.Builder()
        .sslSocketFactory(ssl.socketFactory, trustManager)
        .hostnameVerifier(verifier)
        .connectTimeout(8, TimeUnit.SECONDS)
        .readTimeout(15, TimeUnit.SECONDS)
        .build()
}

/** [address] as OkHttp names it to a hostname verifier: lowercased, IPv6 unbracketed. */
internal fun urlHost(address: String): String =
    mgmtBase(address, DEFAULT_MGMT_PORT).toHttpUrlOrNull()?.host ?: address

/** Parse a PKCS#8 PEM private key (rcgen emits `-----BEGIN PRIVATE KEY-----`), trying EC then RSA/Ed25519. */
private fun parsePrivateKey(pem: String): PrivateKey {
    val body = pem
        .replace(Regex("-----BEGIN [A-Z ]*PRIVATE KEY-----"), "")
        .replace(Regex("-----END [A-Z ]*PRIVATE KEY-----"), "")
        .replace(Regex("\\s"), "")
    val der = Base64.getDecoder().decode(body)
    val spec = PKCS8EncodedKeySpec(der)
    for (alg in listOf("EC", "RSA", "Ed25519")) {
        try {
            return KeyFactory.getInstance(alg).generatePrivate(spec)
        } catch (_: Exception) {
            // try the next algorithm
        }
    }
    throw IllegalArgumentException("unsupported private-key format (not EC/RSA/Ed25519 PKCS#8)")
}

private fun sha256Hex(der: ByteArray): String =
    MessageDigest.getInstance("SHA-256").digest(der).joinToString("") { "%02x".format(it) }
