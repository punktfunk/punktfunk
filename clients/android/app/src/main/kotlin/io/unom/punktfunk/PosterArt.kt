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
import coil3.network.okhttp.OkHttpNetworkFetcherFactory
import coil3.request.ImageRequest
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.mtlsHttpClient
import io.unom.punktfunk.kit.security.ClientIdentity
import java.io.File
import okhttp3.Cache
import okhttp3.OkHttpClient

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
 * proxy, public trust for a CDN, and the one HTTP cache (`cacheDir/art-http`). The proxy sends
 * `Cache-Control` + `ETag`, so a shelf revisit is a 304 at most. Call it off the main thread:
 * the first call opens the cache.
 */
fun posterHttp(context: Context, id: ClientIdentity, address: String, fpHex: String): OkHttpClient =
    mtlsHttpClient(id.certPem, id.privateKeyPem, address, fpHex, ArtCache.get(context))

/** A Coil loader over [posterHttp]. Coil's own disk cache stays off: OkHttp's is the one. */
fun posterLoader(context: Context, id: ClientIdentity, address: String, fpHex: String): ImageLoader =
    ImageLoader.Builder(context)
        .components { add(OkHttpNetworkFetcherFactory(posterHttp(context, id, address, fpHex))) }
        .diskCache(null)
        .build()

/**
 * One [Cache] for the process: OkHttp forbids two on a directory. The first open deletes Coil's
 * `image_cache`, which nothing writes any more.
 */
private object ArtCache {
    private var cache: Cache? = null

    @Synchronized
    fun get(context: Context): Cache = cache ?: run {
        File(context.cacheDir, "image_cache").deleteRecursively()
        Cache(File(context.cacheDir, "art-http"), 64L shl 20).also { cache = it }
    }
}
