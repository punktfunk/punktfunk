package io.unom.punktfunk.kit.library

import android.util.Log
import io.unom.punktfunk.kit.HostText
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.SessionAccess
import io.unom.punktfunk.kit.say
import io.unom.punktfunk.kit.security.ClientIdentity
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import okio.Buffer
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
import java.text.NumberFormat
import java.util.Base64
import java.util.Locale
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
// writes go through [mgmtCall]. Mirrors the GameEntry/Artwork schema in
// crates/host/punktfunk-host/src/library.rs.

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
    /** A title a plugin installs. Null is installed, as every other title is. */
    val install: TitleInstall? = null,
    /** What the host sets for this game's stream; null from an older host or when it sets none. */
    val overrides: GameOverrides? = null,
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
        fun desktop(title: String) = GameEntry(
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
     * (`pf-console-ui::library::store_label`). A store's own name is shown as it is.
     */
    val storeLabel: Say get() = when (store) {
        "steam" -> Say.Text("Steam")
        "custom" -> say(R.string.lib_store_custom)
        "heroic" -> Say.Text("Heroic")
        "lutris" -> Say.Text("Lutris")
        "epic" -> Say.Text("Epic")
        "gog" -> Say.Text("GOG")
        "xbox" -> Say.Text("Xbox")
        else -> say(R.string.lib_store_other)
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
    data class Unauthorized(val message: Say) : LibraryResult()
    data class Error(val message: Say) : LibraryResult()

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
    /** This device's live grants; null from a host that predates the field. */
    val grants: Int? = null,
)

/** A title's files on the host, from the library entry's `install`. */
data class TitleInstall(
    /** `installed` or `missing`; a word this build doesn't know reads as installed. */
    val state: String,
    /** The download size while missing, the size on disk once installed. */
    val sizeBytes: Long? = null,
    /** Free space where the download goes. */
    val freeBytes: Long? = null,
) {
    val missing: Boolean get() = state == "missing"
}

/**
 * What a title menu offers for a title's files; at most one applies. The rules and words are the
 * Rust client's (`pf_client_core::library::InstallAction`).
 */
enum class InstallAction {
    Install, Pause, Resume, Remove;

    /** The menu row: `Install · 26 GB (212 GB free)`, `Remove download · 26 GB`. */
    fun label(install: TitleInstall?): Say {
        val size = install?.sizeBytes?.let { humanBytes(it) }
        return when (this) {
            Install -> {
                val free = install?.freeBytes?.let { humanBytes(it) }
                when {
                    size != null && free != null -> say(R.string.lib_install_size_free, size, free)
                    size != null -> say(R.string.lib_install_size, size)
                    else -> say(R.string.lib_install)
                }
            }
            Pause -> say(R.string.lib_download_pause)
            Resume -> say(R.string.lib_download_resume)
            Remove ->
                if (size != null) say(R.string.lib_download_remove_size, size) else say(R.string.lib_download_remove)
        }
    }

    companion object {
        /** The one row, if any. Null [grants] (an older host) allows starting a download only. */
        fun forTitle(install: TitleInstall?, download: Download?, grants: Int?): InstallAction? {
            if (install == null) return null
            val launch = grants == null || grants and SessionAccess.LAUNCH != 0
            val manage = grants != null && grants and SessionAccess.MANAGE_GAMES != 0
            val action = when (download?.state) {
                "queued", "downloading" -> Pause
                "installing" -> return null
                "paused" -> Resume
                "done" -> Remove
                else -> if (install.missing) Install else Remove
            }
            val allowed = if (action == Install || action == Resume) launch else manage
            return action.takeIf { allowed }
        }
    }
}

/** A tile's mark for a title's files; an installed title has none. Same words as the Rust tiles. */
data class TileBadge(
    /** `download`, `pause` or `alert`; `host` marks the host's stream settings. */
    val icon: String,
    /** `42 %`, `26 GB`, `Not installed`, `Queued`. */
    val text: Say,
) {
    companion object {
        fun forTitle(install: TitleInstall?, download: Download?): TileBadge? {
            val pct = { d: Download ->
                d.fraction?.let { say(R.string.lib_badge_percent, (it * 100).toInt()) }
                    ?: say(R.string.lib_badge_queued)
            }
            return when {
                download != null && download.live -> TileBadge("download", pct(download))
                download?.state == "paused" -> TileBadge("pause", pct(download))
                download?.state == "failed" -> TileBadge("alert", say(R.string.lib_badge_not_installed))
                install?.missing == true ->
                    TileBadge(
                        "download",
                        install.sizeBytes?.let { humanBytes(it) } ?: say(R.string.lib_badge_not_installed),
                    )
                else -> null
            }
        }
    }
}

/** What asking the host to change a title's files came to. The Rust client's `InstallOutcome`. */
sealed class InstallOutcome {
    data object Done : InstallOutcome()

    /** `403`/`404`/`409` with the host's own sentence. */
    data class Refused(val said: Say) : InstallOutcome()

    /** A host without the route. */
    data object Unsupported : InstallOutcome()
    data class Failed(val why: Say) : InstallOutcome()

    /** The player-facing line. */
    fun notice(action: InstallAction, title: String): Say = when (this) {
        Done -> when (action) {
            InstallAction.Install, InstallAction.Resume -> say(R.string.lib_install_started, title)
            InstallAction.Pause -> say(R.string.lib_download_paused_done, title)
            InstallAction.Remove -> say(R.string.lib_removed, title)
        }
        is Refused -> said
        Unsupported -> say(R.string.lib_install_unsupported)
        is Failed -> say(
            when (action) {
                InstallAction.Install -> R.string.lib_install_failed
                InstallAction.Resume -> R.string.lib_resume_failed
                InstallAction.Pause -> R.string.lib_pause_failed
                InstallAction.Remove -> R.string.lib_remove_failed
            },
            title, why,
        )
    }

    companion object {
        /** An answer: the host's sentence when it sent one, else the route is missing. */
        fun fromReply(code: Int, said: Say?): InstallOutcome = when {
            code in 200..299 -> Done
            said != null -> Refused(said)
            code == 401 || code == 404 || code == 405 -> Unsupported
            else -> Failed(say(R.string.error_mgmt_refused, code))
        }
    }
}

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

    /**
     * The segments of `12.3 GB of 26 GB · 48 MB/s · about 4 min left`, or `Installing…`: the
     * screen joins them with " · ".
     */
    fun line(): List<Say> {
        when (state) {
            "queued" -> return listOf(say(R.string.lib_download_queued))
            "installing" -> return listOf(phase?.let { Say.Text(it) } ?: say(R.string.lib_download_installing))
        }
        val total = totalBytes?.takeIf { it > 0 }
        val parts = mutableListOf(
            if (total != null) say(R.string.lib_download_progress, humanBytes(doneBytes), humanBytes(total))
            else say(R.string.lib_download_so_far, humanBytes(doneBytes)),
        )
        if (state == "downloading") {
            rateBps?.let { parts.add(say(R.string.lib_download_rate, humanBytes(it))) }
            etaS?.let {
                parts.add(
                    when {
                        it < 60 -> say(R.string.lib_download_eta_soon)
                        it < 3600 -> say(R.string.lib_download_eta_minutes, (it + 30) / 60)
                        else -> say(R.string.lib_download_eta_hours, it / 3600, (it % 3600) / 60)
                    },
                )
            }
        }
        return parts
    }

    /** Why a launch that waited on it didn't start the title; null while it may still. */
    fun stopped(title: String): Say? = when (state) {
        "failed" ->
            error?.let { say(R.string.lib_download_failed_why, title, it) }
                ?: say(R.string.lib_download_failed, title)
        "cancelled" -> say(R.string.lib_download_cancelled, title)
        "paused" -> say(R.string.lib_download_paused, title)
        else -> null
    }
}

/**
 * Decimal units, as stores count: `12.3 GB`, `48 MB`, `512 kB`. The figure is written for
 * [locale]: no grouping, one decimal below 100.
 */
fun humanBytes(n: Long, locale: Locale = Locale.getDefault()): Say {
    val (value, unit) = when {
        n >= 1_000_000_000L -> n / 1e9 to R.string.common_size_gb
        n >= 1_000_000L -> n / 1e6 to R.string.common_size_mb
        else -> n / 1e3 to R.string.common_size_kb
    }
    val rounded = Math.round(value * 10) / 10.0
    val whole = value >= 100 || rounded % 1.0 == 0.0
    val figure = NumberFormat.getNumberInstance(locale).apply {
        isGroupingUsed = false
        minimumFractionDigits = if (whole) 0 else 1
        maximumFractionDigits = if (whole) 0 else 1
    }
    return say(unit, if (whole) figure.format(Math.round(value)) else figure.format(rounded))
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
    data class Failed(val why: Say) : GameEnd()

    /** The game is gone, so a stream that was playing it can end. */
    val gameGone: Boolean get() = this == Ended || this == NotRunning

    /** The player-facing line. The Rust, Swift and web clients use the same words. */
    fun notice(title: String): Say = when (this) {
        Ended -> say(R.string.lib_end_game_done, title)
        NotRunning -> say(R.string.lib_end_game_not_running, title)
        Unsupported -> say(R.string.lib_end_game_unsupported)
        Expired -> say(R.string.lib_end_game_expired)
        is Failed -> say(R.string.lib_end_game_failed, title, why)
    }

    companion object {
        fun fromStatus(code: Int): GameEnd = when (code) {
            in 200..299 -> Ended
            409 -> NotRunning
            401, 404 -> Unsupported
            403 -> Expired
            else -> Failed(say(R.string.error_mgmt_refused, code))
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
     * The host's catalog, walked by `GET /api/v1/library/page` over [mtlsHttpClient]'s lanes:
     * [streamPort] first, then [mgmtBase] over HTTPS. A host older than the paged route refuses
     * it, so `GET /api/v1/library` answers whole instead. [fpHex] is the pinned host-cert
     * SHA-256 (64 hex, from the paired [io.unom.punktfunk.kit.security.KnownHost]); a blank value
     * means the host was never paired. A refusal maps through [refused]. BLOCKING — call from a
     * background dispatcher.
     */
    fun fetch(
        address: String,
        mgmtPort: Int = DEFAULT_MGMT_PORT,
        certPem: String,
        keyPem: String,
        fpHex: String,
        streamPort: Int = 0,
    ): LibraryResult {
        if (fpHex.isBlank()) {
            return LibraryResult.Unauthorized(say(R.string.android_lib_error_unpaired_first))
        }
        val client = try {
            mtlsHttpClient(certPem, keyPem, address, fpHex, streamPort)
        } catch (e: Exception) {
            Log.w(TAG, "mTLS client for $address", e)
            return LibraryResult.Error(say(R.string.android_lib_error_tls))
        }
        val base = mgmtBase(address, mgmtPort)
        val get = { path: String ->
            client.newCall(Request.Builder().url(base + path).build()).execute()
                .use { it.code to it.body.string() }
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
            LibraryResult.Error(say(R.string.android_lib_error_unreachable))
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
        streamPort: Int = 0,
    ): List<RunningGame> = fetchStatus(address, mgmtPort, certPem, keyPem, fpHex, streamPort).games

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
        streamPort: Int = 0,
    ): HostStatus {
        if (fpHex.isBlank()) return HostStatus()
        return try {
            val client = mtlsHttpClient(certPem, keyPem, address, fpHex, streamPort)
            val req = Request.Builder().url("${mgmtBase(address, mgmtPort)}/api/v1/status").build()
            client.newCall(req).execute().use { resp ->
                if (resp.code != 200) return HostStatus()
                parseStatus(resp.body.string())
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
        id: ClientIdentity,
        address: String,
        mgmtPort: Int,
        fpHex: String,
        appId: String,
        streamPort: Int = 0,
    ): GameEnd {
        if (fpHex.isBlank() || appId.isBlank()) return GameEnd.Failed(say(R.string.android_lib_error_host_unpaired))
        val body = JSONObject().put("app_id", appId).put("streaming", true)
        return when (
            val reply = mgmtCall(id, address, mgmtPort, fpHex, streamPort = streamPort) { base ->
                Request.Builder().url("$base/api/v1/game/end")
                    .post(body.toString().toRequestBody("application/json".toMediaType()))
                    .build()
            }
        ) {
            is MgmtReply.Answer -> GameEnd.fromStatus(reply.code)
            MgmtReply.Unreachable -> GameEnd.Failed(reply.why)
        }
    }

    /**
     * Start, resume, pause or remove a title's download (`/api/v1/library/install/{id}`). A
     * refusal carries the host's own sentence. BLOCKING; call from IO.
     */
    fun changeInstall(
        id: ClientIdentity,
        address: String,
        mgmtPort: Int,
        fpHex: String,
        appId: String,
        action: InstallAction,
        streamPort: Int = 0,
    ): InstallOutcome {
        if (fpHex.isBlank() || appId.isBlank()) {
            return InstallOutcome.Failed(say(R.string.android_lib_error_host_unpaired))
        }
        val path = "/api/v1/library/install/" +
            java.net.URLEncoder.encode(appId, "UTF-8").replace("+", "%20").replace("%3A", ":")
        val empty = ByteArray(0).toRequestBody(null)
        return when (
            val reply = mgmtCall(id, address, mgmtPort, fpHex, streamPort = streamPort) { base ->
                when (action) {
                    InstallAction.Install, InstallAction.Resume -> Request.Builder().url(base + path).post(empty)
                    InstallAction.Pause -> Request.Builder().url("$base$path/pause").post(empty)
                    InstallAction.Remove -> Request.Builder().url(base + path).delete()
                }.build()
            }
        ) {
            is MgmtReply.Answer -> InstallOutcome.fromReply(reply.code, reply.apiError)
            MgmtReply.Unreachable -> InstallOutcome.Failed(reply.why)
        }
    }

    /** A non-200 answer. 401 and 403 both mean "not paired", as on desktop and Apple. */
    internal fun refused(code: Int): LibraryResult =
        if (code == 401 || code == 403) {
            LibraryResult.Unauthorized(say(R.string.error_mgmt_not_paired))
        } else {
            LibraryResult.Error(say(R.string.error_mgmt_refused, code))
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
        streamPort: Int = 0,
        isCancelled: () -> Boolean = { false },
        onWaking: () -> Unit = {},
    ): LibraryResult? = acrossWake(
        waking = autoWake && macs.isNotEmpty(),
        fetch = { fetch(address, mgmtPort, certPem, keyPem, fpHex, streamPort) },
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

    /** The `games[]`, `downloads[]` and `grants` of `/status`; the rest is the console's. */
    internal fun parseStatus(json: String): HostStatus {
        val root = JSONObject(json)
        val downloads = root.optJSONArray("downloads") ?: JSONArray()
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
        val grants = if (root.has("grants") && !root.isNull("grants")) root.optInt("grants") else null
        return HostStatus(parseRunning(json), out, grants)
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
                    install = o.optJSONObject("install")?.let { i ->
                        TitleInstall(
                            state = i.optString("state"),
                            sizeBytes = if (i.has("size_bytes")) i.optLong("size_bytes") else null,
                            freeBytes = if (i.has("free_bytes")) i.optLong("free_bytes") else null,
                        )
                    },
                    overrides = GameOverrides.from(o.optJSONObject("overrides")),
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

/** What one management-API call came to. */
sealed interface MgmtReply {
    /** Why it is not a success, in the user register: the host's own sentence, else its status. */
    val why: Say

    /**
     * The host answered. [apiError] is the sentence of the `ApiError` envelope, when it sent
     * one: its message by code ([HostText]), else its English.
     */
    data class Answer(val code: Int, val body: String, val apiError: Say?) : MgmtReply {
        val ok: Boolean get() = code in 200..299
        override val why: Say get() = apiError ?: say(R.string.error_mgmt_refused, code)

        companion object {
            fun of(code: Int, body: String) = Answer(code, body, HostText.apiError(body))
        }
    }

    /** No answer: the connection, the TLS pin or the request itself failed. The cause is logged. */
    data object Unreachable : MgmtReply {
        override val why: Say get() = say(R.string.android_mgmt_unreachable)
    }
}

/**
 * One request to a paired host's management API over [mtlsHttpClient], [streamPort] first. [build]
 * gets the [mgmtBase] URL. [callTimeoutMs] caps the whole call; zero leaves the client's own
 * timeouts. The exception behind [MgmtReply.Unreachable] goes to the log, never to a person.
 * BLOCKING.
 */
fun mgmtCall(
    id: ClientIdentity,
    addr: String,
    mgmtPort: Int,
    fpHex: String,
    callTimeoutMs: Long = 0,
    streamPort: Int = 0,
    build: (base: String) -> Request,
): MgmtReply = try {
    var client = mtlsHttpClient(id.certPem, id.privateKeyPem, addr, fpHex, streamPort)
    if (callTimeoutMs > 0) client = client.newBuilder().callTimeout(callTimeoutMs, TimeUnit.MILLISECONDS).build()
    client.newCall(build(mgmtBase(addr, mgmtPort))).execute().use { MgmtReply.Answer.of(it.code, it.body.string()) }
} catch (e: Exception) {
    Log.w("pf.mgmt", "mgmt call to $addr", e)
    MgmtReply.Unreachable
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
 * [streamPort] > 0: every request to [host] tries the host's stream port first ([StreamPortFirst]),
 * which needs no TCP 47990.
 *
 * One connection pool per identity, host and pin, shared by every caller: a pool per call leaves
 * each connection idle for five minutes, and the host drops a peer's 33rd connection.
 */
fun mtlsHttpClient(certPem: String, keyPem: String, host: String, fpHex: String, streamPort: Int = 0): OkHttpClient {
    val base = mtlsClients.computeIfAbsent("$host|${fpHex.lowercase()}|$certPem") {
        buildMtlsClient(certPem, keyPem, host, fpHex)
    }
    if (streamPort <= 0) return base
    return base.newBuilder().addInterceptor(StreamPortFirst(host, streamPort, certPem, keyPem, fpHex)).build()
}

/** How long a dial over the stream port waits before HTTPS takes over. */
internal const val STREAM_WAIT_MS = 3_000

/**
 * Sends each request to the pinned [host] over its stream port ([NativeBridge.nativeMgmtRequest])
 * before HTTPS. The stream port's answer stands, except 401 and 403: a host that predates
 * stamping the client certificate on its tunnel refuses the paired lane there. That refusal
 * stands only when HTTPS can't be reached either. Any other origin passes straight through.
 */
private class StreamPortFirst(
    private val host: String,
    private val streamPort: Int,
    private val certPem: String,
    private val keyPem: String,
    private val fpHex: String,
) : Interceptor {
    private val pinnedHost = urlHost(host)

    override fun intercept(chain: Interceptor.Chain): Response {
        val req = chain.request()
        if (req.url.host != pinnedHost) return chain.proceed(req)
        val body = Buffer().also { req.body?.writeTo(it) }.readByteArray()
        val path = req.url.encodedPath + (req.url.encodedQuery?.let { "?$it" } ?: "")
        val contentType = req.body?.contentType()?.toString().orEmpty()
        val tunnelled = NativeBridge.nativeMgmtRequest(
            host, streamPort, certPem, keyPem, fpHex, req.method, path, contentType, body, STREAM_WAIT_MS,
        )?.let(::tunnelReply)?.let { (code, bytes) ->
            Response.Builder().request(req).protocol(Protocol.HTTP_1_1).code(code).message("")
                .body(bytes.toResponseBody()).build()
        }
        if (tunnelled != null && tunnelled.code != 401 && tunnelled.code != 403) return tunnelled
        return try {
            chain.proceed(req).also { tunnelled?.close() }
        } catch (e: java.io.IOException) {
            tunnelled ?: throw e
        }
    }
}

/** A [NativeBridge.nativeMgmtRequest] reply: two bytes of big-endian status, then the body. */
internal fun tunnelReply(reply: ByteArray): Pair<Int, ByteArray>? =
    if (reply.size < 2) null
    else (((reply[0].toInt() and 0xff) shl 8) or (reply[1].toInt() and 0xff)) to reply.copyOfRange(2, reply.size)

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
