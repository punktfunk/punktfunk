package io.unom.punktfunk

import android.content.Context
import android.content.res.Resources
import io.unom.punktfunk.console.SkiaConsole
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.library.GameStats
import io.unom.punktfunk.kit.library.LibraryGroupBy
import io.unom.punktfunk.kit.library.LibrarySection
import io.unom.punktfunk.kit.library.LibrarySections
import io.unom.punktfunk.kit.library.LibrarySortKey
import org.json.JSONArray
import org.json.JSONObject

/** How the touch library arranges a shelf: its sort, its grouping and its sections in order. */
internal data class ShelfLayout(
    val sort: LibrarySortKey = LibrarySortKey.HOST,
    val groupBy: LibraryGroupBy? = null,
    val sections: List<Pair<LibrarySection, Boolean>> = LibrarySections.parse(null),
)

/**
 * The library layout this device keeps. The sort, the sections and each host's favorites live in
 * the settings document the console shell saves (`library_sort`, `library_sections`,
 * `favorites.<fingerprint>`), so both shells show one arrangement. The grouping and the shelf last
 * shown are the touch library's own.
 */
internal object LibraryLayout {
    private const val GROUP_BY = "touch_library_group"
    private const val SHELF = "touch_library_shelf"

    private fun prefs(context: Context) =
        context.applicationContext.getSharedPreferences(SkiaConsole.PREFS, Context.MODE_PRIVATE)

    private fun doc(context: Context): JSONObject =
        prefs(context).getString("json", null)?.let { runCatching { JSONObject(it) }.getOrNull() } ?: JSONObject()

    /**
     * Change the shared document, then hand a running console the new copy: it saves its own
     * whole, and would write the old values back over these.
     */
    private fun write(context: Context, settings: Settings, edit: (JSONObject) -> Unit) {
        val next = doc(context).also(edit)
        prefs(context).edit().putString("json", next.toString()).apply()
        SkiaConsole.settingsChanged(settings)
    }

    fun load(context: Context): ShelfLayout {
        val doc = doc(context)
        return ShelfLayout(
            sort = LibrarySortKey.parse(doc.optString("library_sort")),
            groupBy = LibraryGroupBy.parse(prefs(context).getString(GROUP_BY, null)),
            sections = LibrarySections.parse(doc.optString("library_sections")),
        )
    }

    fun save(context: Context, settings: Settings, layout: ShelfLayout) {
        prefs(context).edit().putString(GROUP_BY, layout.groupBy?.stored).apply()
        write(context, settings) {
            it.put("library_sort", layout.sort.stored)
            it.put("library_sections", LibrarySections.stored(layout.sections))
        }
    }

    /** The titles marked favorite on the host with [fpHex], from this device. */
    fun favorites(context: Context, fpHex: String): Set<String> {
        val list = doc(context).optJSONArray("favorites.$fpHex") ?: return emptySet()
        return (0 until list.length()).mapNotNull { list.optString(it).ifEmpty { null } }.toSet()
    }

    /** Mark or unmark [id]; the set as it now stands. An empty list leaves no key behind. */
    fun toggleFavorite(context: Context, settings: Settings, fpHex: String, id: String): Set<String> {
        val next = favorites(context, fpHex).let { if (id in it) it - id else it + id }
        write(context, settings) {
            if (next.isEmpty()) it.remove("favorites.$fpHex") else it.put("favorites.$fpHex", JSONArray(next))
        }
        return next
    }

    /** A removed host leaves no list of what somebody plays behind. */
    fun forgetFavorites(context: Context, settings: Settings, fpHex: String) {
        if (doc(context).has("favorites.$fpHex")) write(context, settings) { it.remove("favorites.$fpHex") }
    }

    /** The shelf the Library tab last showed: a host's record id, `#preset` after it for a pin. */
    fun shelf(context: Context): String? = prefs(context).getString(SHELF, null)

    fun rememberShelf(context: Context, id: String) = prefs(context).edit().putString(SHELF, id).apply()
}

/** What the host recorded about playing a title, in words: tile captions and the details line. */
internal object PlayStatsText {
    private fun ago(res: Resources, ms: Long): String {
        val min = ms / 60_000
        return when {
            min < 1 -> res.getString(R.string.lib_ago_just_now)
            min < 60 -> res.getString(R.string.lib_ago_minutes, min)
            min < 1_440 -> res.getString(R.string.lib_ago_hours, min / 60)
            min < 43_200 -> res.getString(R.string.lib_ago_days, min / 1_440)
            else -> res.getString(R.string.lib_ago_months, min / 43_200)
        }
    }

    /** `2 h ago`, or null for a title never played. */
    fun lastPlayed(res: Resources, stats: GameStats?, nowMs: Long = System.currentTimeMillis()): String? =
        stats?.lastPlayedUnixMs?.takeIf { it > 0 }?.let { ago(res, (nowMs - it).coerceAtLeast(0)) }

    /** `14 h` in all. Under a minute says nothing: a launch that never ran is not play time. */
    fun playTime(res: Resources, stats: GameStats?): String? {
        val ms = stats?.playTimeMs ?: return null
        return when {
            ms >= 3_600_000 -> res.getString(R.string.lib_played_hours_short, ms / 3_600_000)
            ms >= 60_000 -> res.getString(R.string.lib_played_under_hour)
            else -> null
        }
    }

    /** `Last played 2 h ago · 14 h total · 12 launches`, or null for a title never played. */
    fun line(res: Resources, stats: GameStats?, nowMs: Long = System.currentTimeMillis()): String? {
        val last = stats?.lastPlayedUnixMs?.takeIf { it > 0 } ?: return null
        val min = (nowMs - last).coerceAtLeast(0) / 60_000
        val parts = mutableListOf(
            when {
                min < 1 -> res.getString(R.string.lib_last_played_just_now)
                min < 60 -> res.getString(R.string.lib_last_played_minutes, min)
                min < 1_440 -> res.getString(R.string.lib_last_played_hours, min / 60)
                min < 43_200 -> res.getString(R.string.lib_last_played_days, min / 1_440)
                else -> res.getString(R.string.lib_last_played_months, min / 43_200)
            },
        )
        val hours = stats.playTimeMs / 3_600_000
        if (hours > 0) parts += res.getString(R.string.lib_hours_total, hours)
        if (stats.launchCount > 0) {
            parts += res.getQuantityString(R.plurals.lib_launches, stats.launchCount, stats.launchCount)
        }
        return parts.joinToString(" · ")
    }
}
