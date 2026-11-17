package io.unom.punktfunk

import android.content.Context
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import coil3.ImageLoader
import coil3.compose.AsyncImage
import coil3.disk.DiskCache
import coil3.network.okhttp.OkHttpNetworkFetcherFactory
import coil3.request.ImageRequest
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.mtlsHttpClient
import io.unom.punktfunk.kit.security.ClientIdentity
import java.io.File
import okhttp3.Interceptor
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.Request
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody
import okio.Path.Companion.toOkioPath

/**
 * The poster-candidate walk every poster surface shares: try each URL in order, one at a time,
 * and hand over to [fallback] once the last one has failed. The index resets with the game.
 */
@Composable
fun PosterArt(game: GameEntry, loader: ImageLoader, fallback: @Composable () -> Unit) {
    val candidates = game.art.posterCandidates
    var idx by remember(game.id) { mutableIntStateOf(0) }
    if (idx < candidates.size) {
        AsyncImage(
            model = ImageRequest.Builder(LocalContext.current).data(candidates[idx]).build(),
            imageLoader = loader,
            contentDescription = game.title,
            contentScale = ContentScale.Crop,
            modifier = Modifier.fillMaxSize(),
            onError = { idx++ }, // this candidate failed — try the next, or fall to the placeholder
        )
    } else {
        fallback()
    }
}

/**
 * The client every poster fetch goes through, in both shells: the host's pin for its own art
 * proxy, tried over [streamPort] first, public trust for a CDN, and [PosterCache] above both.
 * Call it off the main thread: the first call opens the cache.
 */
fun posterHttp(context: Context, id: ClientIdentity, address: String, fpHex: String, streamPort: Int): OkHttpClient =
    mtlsHttpClient(id.certPem, id.privateKeyPem, address, fpHex, streamPort).newBuilder()
        .apply { interceptors().add(0, PosterDiskCache(PosterCache.get(context))) }
        .build()

/** A Coil loader over [posterHttp]. Coil's own disk cache stays off: [PosterCache] is the one. */
fun posterLoader(context: Context, id: ClientIdentity, address: String, fpHex: String, streamPort: Int): ImageLoader =
    ImageLoader.Builder(context)
        .components { add(OkHttpNetworkFetcherFactory(posterHttp(context, id, address, fpHex, streamPort))) }
        .diskCache(null)
        .build()

/**
 * Poster bytes by URL, one cache for the process: two must not share a directory. It sits above
 * the transport because a poster the stream port served never passes OkHttp's own cache. The
 * first open deletes `art-http`, the old HTTP cache, and Coil's `image_cache`.
 */
private object PosterCache {
    private var cache: DiskCache? = null

    @Synchronized
    fun get(context: Context): DiskCache = cache ?: run {
        File(context.cacheDir, "image_cache").deleteRecursively()
        File(context.cacheDir, "art-http").deleteRecursively()
        DiskCache.Builder().directory(File(context.cacheDir, "posters").toOkioPath())
            .maxSizeBytes(64L shl 20).build().also { cache = it }
    }
}

/** 16 MiB. A body past it is not a poster, and is neither kept nor read whole. */
private const val POSTER_MAX_BYTES = 16L shl 20

/**
 * A GET answered from [cache] when it holds the URL, else from the network and kept on a 200. A
 * request for the cache alone (`CacheControl.FORCE_CACHE`) misses with a 504, as OkHttp's does.
 */
internal class PosterDiskCache(private val cache: DiskCache) : Interceptor {
    override fun intercept(chain: Interceptor.Chain): Response {
        val req = chain.request()
        if (req.method != "GET") return chain.proceed(req)
        val key = req.url.toString()
        val kept = runCatching {
            cache.openSnapshot(key)?.use { cache.fileSystem.read(it.data) { readByteArray() } }
        }.getOrNull()
        if (kept != null) return answer(req, 200, kept)
        if (req.cacheControl.onlyIfCached) return answer(req, 504, ByteArray(0))
        val resp = chain.proceed(req)
        if (resp.code != 200 || resp.body.contentLength() > POSTER_MAX_BYTES) return resp
        // A body of no stated length is buffered no further than the cap.
        if (resp.body.source().request(POSTER_MAX_BYTES + 1)) return resp
        val type = resp.body.contentType()
        val bytes = resp.body.bytes()
        cache.openEditor(key)?.let { editor ->
            runCatching {
                cache.fileSystem.write(editor.data) { write(bytes) }
                editor.commit()
            }.onFailure { editor.abort() }
        }
        return resp.newBuilder().body(bytes.toResponseBody(type)).build()
    }

    private fun answer(req: Request, code: Int, body: ByteArray): Response =
        Response.Builder().request(req).protocol(Protocol.HTTP_1_1).code(code).message("")
            .body(body.toResponseBody()).build()
}
