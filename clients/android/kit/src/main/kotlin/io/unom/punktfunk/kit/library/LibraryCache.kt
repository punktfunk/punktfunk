package io.unom.punktfunk.kit.library

import io.unom.punktfunk.kit.security.KnownHost
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.security.MessageDigest

// On-disk cache for a host's library CATALOG — the list of titles, not their art. The Android
// mirror of the Apple client's `LibraryCache.swift` and the Rust `pf_client_core::library_cache`.
//
// Cover art has been cached on disk for a while; the catalog behind it never was. Every visit to a
// library refetched `GET /api/v1/library` and showed a spinner until that call returned. A host
// that is asleep, or simply not reachable yet, therefore had an EMPTY library — which is the
// opposite of what a player wants from the screen they use to decide what to play, and it makes
// waking a host on library entry pointless: there would be nothing to look at while it boots.
//
// So the catalog is cached per host and rendered immediately, marked stale, and reconciled when the
// host answers.
//
// Cache directory (`context.cacheDir`), not files: every byte is re-derivable from the host, so
// Android is welcome to evict it under storage pressure. Unlike art, a catalog is small (a few
// hundred KB for a big library), so there is no size budget here — one file per host and per
// profile it was fetched as, replaced wholesale.
//
// Takes a plain [File] directory rather than a Context so it can be unit-tested against a temp
// dir, exactly like the Apple original.

/** A host's library as last seen, with when that was (epoch millis). */
data class CachedLibrary(val games: List<GameEntry>, val fetchedAt: Long)

class LibraryCache(private val directory: File) {

    /**
     * This host's last-known catalog as [profile] plays it (null: the box's), or null if there is
     * no usable one. A shelf never shows another profile's titles.
     *
     * A catalog written by an older build whose [GameEntry] had different fields decodes to null
     * rather than throwing: a miss costs one fetch, which is what would have happened anyway.
     * Never surfaced as an error.
     */
    fun load(hostKey: String, profile: String?): CachedLibrary? = try {
        val file = pathFor(hostKey, profile)
        if (!file.isFile) {
            null
        } else {
            val root = JSONObject(file.readText())
            val arr = root.getJSONArray("games")
            val games = ArrayList<GameEntry>(arr.length())
            for (i in 0 until arr.length()) {
                games.add(decode(arr.getJSONObject(i)))
            }
            CachedLibrary(games, root.optLong("fetchedAt"))
        }
    } catch (_: Exception) {
        null
    }

    /**
     * Remember this host's catalog as fetched for [profile]. Best-effort: a cache that can't write
     * is a slower app, not a broken one, so every failure here is swallowed.
     */
    fun store(hostKey: String, profile: String?, games: List<GameEntry>) {
        // An empty catalog is not worth remembering: it is indistinguishable from "never fetched"
        // when read back, and caching it would pin a blank library over a host that has titles.
        if (games.isEmpty()) return
        try {
            val root = JSONObject()
                .put("fetchedAt", System.currentTimeMillis())
                .put("games", JSONArray().apply { games.forEach { put(encode(it)) } })
            // Write-then-rename: an app killed mid-write must not leave a half-file that the next
            // launch reads as a corrupt catalog.
            val target = pathFor(hostKey, profile)
            target.parentFile?.mkdirs()
            val tmp = File(target.parentFile, "${target.name}.tmp")
            tmp.writeText(root.toString())
            if (!tmp.renameTo(target)) {
                tmp.delete()
            }
        } catch (_: Exception) {
            // a cache that can't write is a slower app, not a broken one
        }
    }

    /**
     * Drop a host's catalogs, every profile's — part of forgetting the host, so a removed host
     * leaves no list of what somebody plays behind on the device.
     */
    fun forget(hostKey: String) {
        runCatching { pathFor(hostKey, null).delete() }
        runCatching { File(directory, hashed(hostKey)).deleteRecursively() }
    }

    /**
     * `<host>.json` for the box, the name a catalog had before profiles, else
     * `<host>/<profile>.json`. Hashed rather than used verbatim: a host key is user-controlled
     * text (a name, an address) and must never be able to reach out of this directory (`../`)
     * or exceed a filename length limit.
     */
    internal fun pathFor(hostKey: String, profile: String?): File =
        if (profile == null) File(directory, hashed(hostKey) + ".json")
        else File(File(directory, hashed(hostKey)), hashed(profile) + ".json")

    private fun hashed(text: String): String =
        MessageDigest.getInstance("SHA-256").digest(text.toByteArray()).joinToString("") { "%02x".format(it) }

    /**
     * The wire shape, so a cached file and a host response decode through the same [GameEntry].
     * Art URLs are stored ALREADY RESOLVED to absolute — the host-relative form only means
     * anything next to the base it was fetched from, and re-deriving that base on load would make
     * a cache entry depend on the address the host happens to have today.
     */
    private fun encode(g: GameEntry): JSONObject = JSONObject()
        .put("id", g.id)
        .put("store", g.store)
        .put("title", g.title)
        .put("art", JSONObject().apply {
            g.art.portrait?.let { put("portrait", it) }
            g.art.header?.let { put("header", it) }
            g.art.hero?.let { put("hero", it) }
        })
        .apply {
            g.role?.let { put("role", it) }
            g.icon?.let { put("icon", it) }
            g.platform?.let { put("platform", it) }
            g.developer?.let { put("developer", it) }
            g.releaseYear?.let { put("release_year", it) }
            if (g.genres.isNotEmpty()) put("genres", JSONArray(g.genres))
            g.stats?.let { put("stats", it.toJson()) }
            g.overrides?.let { put("overrides", it.toJson()) }
        }

    private fun decode(o: JSONObject): GameEntry {
        val art = o.optJSONObject("art") ?: JSONObject()
        return GameEntry(
            id = o.optString("id"),
            store = o.optString("store"),
            title = o.optString("title"),
            art = Artwork(
                portrait = nullable(art, "portrait"),
                header = nullable(art, "header"),
                hero = nullable(art, "hero"),
            ),
            role = nullable(o, "role"),
            icon = nullable(o, "icon"),
            platform = nullable(o, "platform"),
            developer = nullable(o, "developer"),
            releaseYear = o.optInt("release_year").takeIf { it > 0 },
            genres = o.optJSONArray("genres")
                ?.let { a -> List(a.length()) { a.optString(it) }.filter { it.isNotBlank() } }
                ?: emptyList(),
            stats = GameStats.from(o.optJSONObject("stats")),
            overrides = GameOverrides.from(o.optJSONObject("overrides")),
        )
    }

    private fun nullable(o: JSONObject, key: String): String? =
        if (o.has(key) && !o.isNull(key)) o.optString(key).ifBlank { null } else null

    companion object {
        /** The app's standard location for this cache, under Android's evictable cache dir. */
        fun standard(cacheDir: File): LibraryCache = LibraryCache(File(cacheDir, "punktfunk-library"))

        /**
         * The key a host's catalog is filed under, in both shells: the saved record's id, so a
         * re-addressed host keeps its shelf, else the pin. An unpinned host is never fetched, so
         * it never needs a key of its own.
         */
        fun keyFor(saved: KnownHost?, fpHex: String): String = saved?.id ?: fpHex
    }
}
