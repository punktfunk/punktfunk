package io.unom.punktfunk.console

import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.InstallAction
import io.unom.punktfunk.kit.library.InstallOutcome
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.LibraryResult
import io.unom.punktfunk.kit.library.mgmtBase
import io.unom.punktfunk.kit.pictureUrl
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.posterHttp
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicLong
import okhttp3.CacheControl
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONObject

// The console's library pipeline, run by [SkiaConsole]: the shelf fetch and its posters, and the
// two title writes (end a game, change a download), each followed by a fresh read of the host.

private val artPool = Executors.newFixedThreadPool(3) { r -> Thread(r, "pf-console-art").apply { isDaemon = true } }

/** The library fetch in flight (its generation; a newer one supersedes it). */
private val fetchGen = AtomicLong(0)

/** End a title this device launched, say how it went, then re-read what the host runs. */
internal fun SkiaConsole.endGame(c: JSONObject) {
    val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
    val appId = c.optString("app_id"); val title = c.optString("title")
    val id = identity
    if (id == null) {
        notice(identities.blockedMessage())
        return
    }
    ioPool.execute {
        val outcome = LibraryClient.endGame(id, addr, mgmt, fp, appId, streamPort(fp))
        state.launch {
            notice(outcome.notice(title))
            fetchLibrary(c, refreshOnly = true)
        }
    }
}

/**
 * Start, resume, pause or remove a title's download, say how it went, then re-read the host:
 * the whole catalog after a removal (the title's tile turns to "not installed"), else `/status`.
 */
internal fun SkiaConsole.changeInstall(c: JSONObject) {
    val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
    val appId = c.optString("app_id"); val title = c.optString("title")
    val action = runCatching { InstallAction.valueOf(c.optString("action")) }.getOrNull() ?: return
    val id = identity
    if (id == null) {
        notice(identities.blockedMessage())
        return
    }
    ioPool.execute {
        val outcome = LibraryClient.changeInstall(id, addr, mgmt, fp, appId, action, streamPort(fp))
        state.launch {
            notice(outcome.notice(action, title))
            val removed = outcome == InstallOutcome.Done && action == InstallAction.Remove
            fetchLibrary(c, refreshOnly = !removed)
        }
    }
}

/**
 * The library pipeline (the desktop's `spawn_fetch`): cached shelf first, then
 * [LibraryClient.fetchAcrossWake], then the running set and the posters — each poster
 * fetched over the same mTLS client and pushed as bytes. The list and phase go up under the
 * command's `req`; a refetch this shell starts reuses the last one, and only for its host.
 * A wake takes the MACs of the record the pin names, else of the placeholder at `addr` and
 * `port`, the stream port ([io.unom.punktfunk.kit.security.KnownHostStore.resolve]).
 */
internal fun SkiaConsole.fetchLibrary(c: JSONObject, refreshOnly: Boolean) {
    val app = appContext ?: return
    val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
    val id = identity
    val kh = services.store.resolve(fp, addr, c.optInt("port"))
    val port = kh?.port ?: c.optInt("port")
    if (refreshOnly) {
        // Silent path — no notice, but the failed load still gets its retry.
        if (id == null) { identities.blockedMessage(); return }
        // A newer fetch owns the shelf by the time a slow host answers: not its titles.
        val gen = fetchGen.get()
        ioPool.execute {
            val status = LibraryClient.fetchStatus(addr, mgmt, id.certPem, id.privateKeyPem, fp, port)
            val games = status.games
            state.launch {
                if (handle == 0L) return@launch
                if (gen == fetchGen.get()) {
                    NativeBridge.nativeConsoleLibraryDownloads(
                        handle,
                        ConsoleJson.downloads(status.downloads, status.grants),
                    )
                    NativeBridge.nativeConsoleLibraryRunning(handle, ConsoleJson.runningGames(games))
                }
                // The carousel behind the shelf shows the same fact from its own map; this
                // answer is fresher than anything its TTL would fetch.
                services.nowPlayingAt[fp] = android.os.SystemClock.elapsedRealtime()
                recordNowPlaying(fp, games)
            }
        }
        return
    }
    val req = state.now {
        c.optJSONObject("req")?.also { libraryReq = fp to it } ?: libraryReq?.takeIf { it.first == fp }?.second
    } ?: return // the console has moved off this host: no shelf to refill
    val gen = fetchGen.incrementAndGet()
    NativeBridge.nativeConsoleLibraryBegin(handle)
    if (id == null) {
        NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError(req, "Couldn't load the library", identities.blockedMessage(), true))
        return
    }
    val cache = LibraryCache.standard(app.cacheDir)
    val cacheKey = LibraryCache.keyFor(kh, fp)
    val autoWake = state.now { settings.autoWakeEnabled }
    ioPool.execute {
        val cached = cache.load(cacheKey)?.games?.takeIf { it.isNotEmpty() }
        if (cached != null) state.launch { if (gen == fetchGen.get()) NativeBridge.nativeConsoleLibraryGames(handle, ConsoleJson.libraryGames(req, cached), true) }
        val result = LibraryClient.fetchAcrossWake(
            addr, mgmt, id.certPem, id.privateKeyPem, fp,
            macs = kh?.mac.orEmpty(),
            autoWake = autoWake,
            streamPort = port,
            isCancelled = { gen != fetchGen.get() },
            onWaking = { state.launch { if (gen == fetchGen.get()) NativeBridge.nativeConsoleLibraryStale(handle, 1) } },
        )
        if (gen != fetchGen.get()) return@execute
        when (val r = result) {
            is LibraryResult.Ok -> {
                val games = r.games
                cache.store(cacheKey, games)
                val up = LibraryClient.fetchRunning(addr, mgmt, id.certPem, id.privateKeyPem, fp, port)
                state.launch {
                    if (gen != fetchGen.get()) return@launch
                    NativeBridge.nativeConsoleLibraryGames(handle, ConsoleJson.libraryGames(req, games), false)
                    NativeBridge.nativeConsoleLibraryStale(handle, 0)
                    NativeBridge.nativeConsoleLibraryRunning(handle, ConsoleJson.runningGames(up))
                }
                pumpArt(games, gen, id, addr, fp, offline = false)
            }
            is LibraryResult.Unauthorized -> {
                if (cached != null) pumpArt(cached, gen, id, addr, fp, offline = true)
                state.launch {
                    if (gen != fetchGen.get()) return@launch
                    if (cached != null) NativeBridge.nativeConsoleLibraryStale(handle, 2)
                    else NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError(req, "Not paired", r.message, false))
                }
            }
            is LibraryResult.Error -> {
                if (cached != null) pumpArt(cached, gen, id, addr, fp, offline = true)
                state.launch {
                    if (gen != fetchGen.get()) return@launch
                    if (cached != null) NativeBridge.nativeConsoleLibraryStale(handle, 2)
                    else NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError(req, "Couldn't load the library", r.message, true))
                }
            }
            null -> {}
        }
    }
}

/**
 * Every poster on this shelf, one job each, over the touch shelf's client and cache
 * ([posterHttp]). The console gets encoded bytes because Skia decodes them itself.
 *
 * Also runs when the host did not answer: the shelf is drawn from the library cache and
 * the covers for it are on disk too, so a lettered placeholder next to "last known
 * library" is a picture thrown away rather than one we never had.
 */
private fun SkiaConsole.pumpArt(
    games: List<GameEntry>,
    gen: Long,
    id: ClientIdentity,
    addr: String,
    fp: String,
    offline: Boolean,
) {
    val app = appContext ?: return
    val http = runCatching { posterHttp(app, id, addr, fp, streamPort(fp)) }.getOrNull() ?: return
    for (g in games) {
        val candidates = g.art.posterCandidates
        if (candidates.isEmpty()) continue
        artPool.execute {
            if (gen != fetchGen.get()) return@execute
            val bytes = fetchArt(candidates, http, offline) ?: return@execute
            state.launch { if (gen == fetchGen.get() && handle != 0L) NativeBridge.nativeConsoleLibraryArt(handle, g.id, bytes) }
        }
    }
}

/** `ConsoleCmd::FetchAvatar`: a profile's picture, over the poster client and its cache. */
internal fun SkiaConsole.fetchAvatar(c: JSONObject) {
    val addr = c.optString("addr"); val fp = c.optString("fp_hex"); val path = c.optString("url")
    val id = identity ?: return
    val app = appContext ?: return
    val url = pictureUrl(mgmtBase(addr, c.optInt("mgmt")), path) ?: return
    artPool.execute {
        val http = runCatching { posterHttp(app, id, addr, fp, streamPort(fp)) }.getOrNull() ?: return@execute
        val bytes = fetchArt(listOf(url), http, offline = false) ?: return@execute
        state.launch { if (handle != 0L) NativeBridge.nativeConsoleAvatar(handle, path, bytes) }
    }
}

/**
 * One poster: the candidates in order, first success wins. Any failure, a malformed URL
 * included, moves on to the next candidate; none left is no cover.
 */
internal fun fetchArt(candidates: List<String>, client: OkHttpClient, offline: Boolean): ByteArray? {
    for (url in candidates) {
        val bytes = runCatching {
            val req = Request.Builder().url(url)
            // With the host down the cache is the only answer there is. Left to itself OkHttp
            // honours the proxy's `max-age`, goes to revalidate once it lapses, fails to
            // connect, and reports a miss on bytes that are sitting on disk.
            if (offline) req.cacheControl(CacheControl.FORCE_CACHE)
            client.newCall(req.build()).execute().use { resp ->
                if (resp.code == 200) resp.body.bytes().takeIf { it.isNotEmpty() && it.size <= 16 shl 20 } else null
            }
        }.getOrNull()
        if (bytes != null) return bytes
    }
    return null
}
