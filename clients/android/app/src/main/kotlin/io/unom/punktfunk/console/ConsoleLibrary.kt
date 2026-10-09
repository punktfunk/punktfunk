package io.unom.punktfunk.console

import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.InstallAction
import io.unom.punktfunk.kit.library.InstallOutcome
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.LibraryResult
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
        val outcome = LibraryClient.endGame(id, addr, mgmt, fp, appId)
        main.post {
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
        val outcome = LibraryClient.changeInstall(id, addr, mgmt, fp, appId, action)
        main.post {
            notice(outcome.notice(action, title))
            val removed = outcome == InstallOutcome.Done && action == InstallAction.Remove
            fetchLibrary(c, refreshOnly = !removed)
        }
    }
}

/**
 * The library pipeline (the desktop's `spawn_fetch`): cached shelf first, then
 * [LibraryClient.fetchAcrossWake], then the running set and the posters — each poster
 * fetched over the same mTLS client and pushed as bytes.
 */
internal fun SkiaConsole.fetchLibrary(c: JSONObject, refreshOnly: Boolean) {
    val app = appContext ?: return
    val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
    val id = identity
    val kh = knownHostStore.getByFp(fp)
    if (refreshOnly) {
        // Silent path — no notice, but the failed load still gets its retry.
        if (id == null) { identities.blockedMessage(); return }
        // A newer fetch owns the shelf by the time a slow host answers: not its titles.
        val gen = fetchGen.get()
        ioPool.execute {
            val status = LibraryClient.fetchStatus(addr, mgmt, id.certPem, id.privateKeyPem, fp)
            val games = status.games
            main.post {
                if (handle == 0L) return@post
                if (gen == fetchGen.get()) {
                    NativeBridge.nativeConsoleLibraryDownloads(
                        handle,
                        ConsoleJson.downloads(status.downloads, status.grants),
                    )
                    NativeBridge.nativeConsoleLibraryRunning(handle, ConsoleJson.runningGames(games))
                }
                // The carousel behind the shelf shows the same fact from its own map; this
                // answer is fresher than anything its TTL would fetch.
                nowPlayingAt[fp] = android.os.SystemClock.elapsedRealtime()
                recordNowPlaying(fp, games)
            }
        }
        return
    }
    val gen = fetchGen.incrementAndGet()
    NativeBridge.nativeConsoleLibraryBegin(handle)
    if (id == null) {
        NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError("Couldn't load the library", identities.blockedMessage(), true))
        return
    }
    val cache = LibraryCache.standard(app.cacheDir)
    val cacheKey = LibraryCache.keyFor(kh, fp)
    ioPool.execute {
        val cached = cache.load(cacheKey)?.games?.takeIf { it.isNotEmpty() }
        if (cached != null) main.post { if (gen == fetchGen.get()) NativeBridge.nativeConsoleLibraryGames(handle, ConsoleJson.libraryGames(cached), true) }
        val result = LibraryClient.fetchAcrossWake(
            addr, mgmt, id.certPem, id.privateKeyPem, fp,
            macs = kh?.mac.orEmpty(),
            autoWake = settings.autoWakeEnabled,
            isCancelled = { gen != fetchGen.get() },
            onWaking = { main.post { if (gen == fetchGen.get()) NativeBridge.nativeConsoleLibraryStale(handle, 1) } },
        )
        if (gen != fetchGen.get()) return@execute
        when (val r = result) {
            is LibraryResult.Ok -> {
                val games = r.games
                cache.store(cacheKey, games)
                val up = LibraryClient.fetchRunning(addr, mgmt, id.certPem, id.privateKeyPem, fp)
                main.post {
                    if (gen != fetchGen.get()) return@post
                    NativeBridge.nativeConsoleLibraryGames(handle, ConsoleJson.libraryGames(games), false)
                    NativeBridge.nativeConsoleLibraryStale(handle, 0)
                    NativeBridge.nativeConsoleLibraryRunning(handle, ConsoleJson.runningGames(up))
                }
                pumpArt(games, gen, id, addr, fp, offline = false)
            }
            is LibraryResult.Unauthorized -> {
                if (cached != null) pumpArt(cached, gen, id, addr, fp, offline = true)
                main.post {
                    if (gen != fetchGen.get()) return@post
                    if (cached != null) NativeBridge.nativeConsoleLibraryStale(handle, 2)
                    else NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError("Not paired", r.message, false))
                }
            }
            is LibraryResult.Error -> {
                if (cached != null) pumpArt(cached, gen, id, addr, fp, offline = true)
                main.post {
                    if (gen != fetchGen.get()) return@post
                    if (cached != null) NativeBridge.nativeConsoleLibraryStale(handle, 2)
                    else NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError("Couldn't load the library", r.message, true))
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
    val http = runCatching { posterHttp(app, id, addr, fp) }.getOrNull() ?: return
    for (g in games) {
        val candidates = g.art.posterCandidates
        if (candidates.isEmpty()) continue
        artPool.execute {
            if (gen != fetchGen.get()) return@execute
            val bytes = fetchArt(candidates, http, offline) ?: return@execute
            main.post { if (gen == fetchGen.get() && handle != 0L) NativeBridge.nativeConsoleLibraryArt(handle, g.id, bytes) }
        }
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
