package io.unom.punktfunk.kit.library

import java.util.Locale

// The library's sort and group rules, the same as the desktop's collate.rs and the Apple client's
// LibraryCollation.swift. `clients/shared/library-collate-vectors.json` pins them. Every ordering
// is bytewise over UTF-8 (Rust's `str::cmp`), never locale-aware.

/** How titles are ordered within a group. [stored] is the persisted `library_sort` string. */
enum class LibrarySortKey(val stored: String) {
    HOST("host"),
    TITLE("title"),
    PLATFORM("platform"),
    STORE("store"),
    RECENT("recent"),
    PLAYTIME("playtime");

    companion object {
        /** An unknown string is a newer client's key; it reads as the host's own order. */
        fun parse(stored: String?): LibrarySortKey = entries.firstOrNull { it.stored == stored } ?: HOST
    }
}

/** What to bucket by; null at the call sites is one group holding everything. */
enum class LibraryGroupBy(val stored: String) {
    PLATFORM("platform"),
    STORE("store");

    companion object {
        fun parse(stored: String?): LibraryGroupBy? = entries.firstOrNull { it.stored == stored }
    }
}

/** What a group is, as data: the names are ids that a screen words. */
sealed interface LibraryGroupKey {
    data object Launchers : LibraryGroupKey
    data class Platform(val name: String) : LibraryGroupKey
    data class Store(val name: String) : LibraryGroupKey
}

/** One collated bucket; [indices] point into the list handed to [LibraryCollation.collate]. */
data class LibraryGroup(val key: LibraryGroupKey, val indices: List<Int>)

object LibraryCollation {
    /** The platform bucket of a title with neither a platform nor a known store. */
    const val OTHER_PLATFORM = "Other"

    /** The one bucket of an ungrouped shelf. */
    const val ALL = "All"

    /**
     * A title's A-Z key: the search fold, Latin accents relaxed, punctuation dropped, a leading
     * English article removed unless that would leave nothing.
     */
    fun sortTitle(title: String): String {
        val relaxed = StringBuilder()
        searchFold(title).forEachCodePoint { cp ->
            val folded = LATIN_RELAXED[cp] ?: cp
            if (isAlphanumeric(folded) || isWhiteSpace(folded)) relaxed.appendCodePoint(folded)
        }
        val trimmed = relaxed.toString().trimWhiteSpace()
        for (article in ARTICLES) {
            if (!trimmed.startsWith(article)) continue
            val rest = trimmed.substring(article.length).trimWhiteSpace()
            if (rest.isNotEmpty()) return rest
        }
        return trimmed
    }

    /**
     * `punktfunk_core::search::fold`, scalar for scalar: lowercase, full-width ASCII and the
     * ideographic space as ASCII, half-width kana at full width, a kana and a combining voiced
     * mark composed, conjoining jamo composed, then katakana as hiragana. The prolonged sound
     * mark keeps its code point.
     */
    fun searchFold(text: String): String {
        val out = StringBuilder()
        // Held back one scalar: the next may be a mark or a jamo that composes with it.
        var held = -1
        text.lowercase(Locale.ROOT).forEachCodePoint { cp ->
            val c = compat(cp)
            val both = if (held >= 0) compose(held, c) else null
            if (both != null) {
                held = both
            } else {
                if (held >= 0) out.appendCodePoint(hiragana(held))
                held = c
            }
        }
        if (held >= 0) out.appendCodePoint(hiragana(held))
        return out.toString()
    }

    /**
     * Collates [games] into display groups, as indices into [games].
     *
     * Launchers lead whatever the sort and keep the host's order; the desktop tile is no title
     * and belongs to no group. Groups run A-Z by name, ties in insertion order. Sorting applies
     * within a group, and every comparison falls back to the index, so equal keys keep host order.
     */
    fun collate(
        games: List<GameEntry>,
        sort: LibrarySortKey,
        groupBy: LibraryGroupBy?,
        storeLabel: (GameEntry) -> String,
    ): List<LibraryGroup> {
        val launchers = ArrayList<Int>()
        val buckets = LinkedHashMap<LibraryGroupKey, MutableList<Int>>()
        games.forEachIndexed { i, game ->
            when {
                game.isDesktop -> Unit
                game.isLauncher -> launchers += i
                else -> {
                    val key = groupBy?.let { bucket(game, it, storeLabel) }
                        ?: LibraryGroupKey.Platform(ALL)
                    buckets.getOrPut(key) { ArrayList() } += i
                }
            }
        }

        val titleKeys = when (sort) {
            LibrarySortKey.TITLE, LibrarySortKey.PLATFORM, LibrarySortKey.STORE ->
                games.map { sortTitle(it.title) }
            else -> emptyList()
        }
        val storeLabels = if (sort == LibrarySortKey.STORE) games.map(storeLabel) else emptyList()
        fun byTitle(a: Int, b: Int) = bytewise(titleKeys[a], titleKeys[b])
        val precedes = Comparator<Int> { a, b ->
            val primary = when (sort) {
                LibrarySortKey.HOST -> 0
                LibrarySortKey.TITLE -> byTitle(a, b)
                LibrarySortKey.PLATFORM ->
                    bytewise(games[a].platform.orEmpty(), games[b].platform.orEmpty())
                        .takeIf { it != 0 } ?: byTitle(a, b)
                LibrarySortKey.STORE ->
                    bytewise(storeLabels[a], storeLabels[b]).takeIf { it != 0 } ?: byTitle(a, b)
                LibrarySortKey.RECENT ->
                    (games[b].stats?.lastPlayedUnixMs ?: 0).compareTo(games[a].stats?.lastPlayedUnixMs ?: 0)
                LibrarySortKey.PLAYTIME ->
                    (games[b].stats?.playTimeMs ?: 0).compareTo(games[a].stats?.playTimeMs ?: 0)
            }
            if (primary != 0) primary else a.compareTo(b)
        }

        val out = ArrayList<LibraryGroup>()
        if (launchers.isNotEmpty()) out += LibraryGroup(LibraryGroupKey.Launchers, launchers)
        // `sortedWith` is stable, so equal names keep the order the library first mentioned them.
        buckets.entries
            .sortedWith { a, b -> bytewise(a.key.name, b.key.name) }
            .forEach { (key, indices) -> out += LibraryGroup(key, indices.sortedWith(precedes)) }
        return out
    }

    /** Rust's `str::cmp`: UTF-8 byte order, which is code point order. */
    internal fun bytewise(a: String, b: String): Int {
        var i = 0
        var j = 0
        while (i < a.length && j < b.length) {
            val x = a.codePointAt(i)
            val y = b.codePointAt(j)
            if (x != y) return x.compareTo(y)
            i += Character.charCount(x)
            j += Character.charCount(y)
        }
        return (i < a.length).compareTo(j < b.length)
    }

    /**
     * A platform-less game files under its store, not under "Unknown": a Steam library has no
     * platforms at all. Only a title with neither lands in [OTHER_PLATFORM].
     */
    private fun bucket(
        game: GameEntry,
        by: LibraryGroupBy,
        storeLabel: (GameEntry) -> String,
    ): LibraryGroupKey = when (by) {
        LibraryGroupBy.PLATFORM -> {
            val platform = game.platform?.trim().orEmpty()
            when {
                platform.isNotEmpty() -> LibraryGroupKey.Platform(platform)
                game.hasStoreName -> LibraryGroupKey.Store(storeLabel(game))
                else -> LibraryGroupKey.Platform(OTHER_PLATFORM)
            }
        }
        LibraryGroupBy.STORE -> LibraryGroupKey.Store(storeLabel(game))
    }

    private val LibraryGroupKey.name: String
        get() = when (this) {
            LibraryGroupKey.Launchers -> ""
            is LibraryGroupKey.Platform -> name
            is LibraryGroupKey.Store -> name
        }

    private val ARTICLES = listOf("the ", "a ", "an ")

    private val LATIN_RELAXED: Map<Int, Int> = buildMap {
        val table = mapOf(
            'a' to "áàâäãå",
            'e' to "éèêë",
            'i' to "íìîï",
            'o' to "óòôöõ",
            'u' to "úùûü",
            'c' to "ç",
            'n' to "ñ",
        )
        for ((plain, accented) in table) accented.forEach { put(it.code, plain.code) }
    }

    /** U+FF61-FF9F as NFKC maps them; the last two are the combining voiced marks. */
    private val HALF_WIDTH_KANA: IntArray = (
        "。「」、・ヲァィゥェォャュョッ" +
            "ーアイウエオカキクケコサシス" +
            "セソタチツテトナニヌネノハヒ" +
            "フヘホマミムメモヤユヨラリル" +
            "レロワン゙゚"
        ).codePointsArray()

    private fun compat(c: Int): Int = when (c) {
        0x3000 -> 0x20
        in 0xFF01..0xFF5E -> c - 0xFEE0
        in 0xFF61..0xFF9F -> HALF_WIDTH_KANA[c - 0xFF61]
        else -> c
    }

    /** The precomposed character canonical composition makes of [a] then [b], if any. */
    private fun compose(a: Int, b: Int): Int? = when {
        b in 0x1161..0x1175 && a in 0x1100..0x1112 ->
            0xAC00 + ((a - 0x1100) * 21 + (b - 0x1161)) * 28
        b in 0x11A8..0x11C2 && a in 0xAC00..0xD7A3 && (a - 0xAC00) % 28 == 0 -> a + (b - 0x11A7)
        b == 0x3099 -> voiced(a)
        b == 0x309A && isHaRow(kanaRow(a)) -> a + 2
        else -> null
    }

    /** Katakana sits 0x60 above hiragana, row for row. */
    private fun kanaRow(a: Int): Int = if (a in 0x30A1..0x30FE) a - 0x60 else a

    /** The one row that takes both marks. */
    private fun isHaRow(h: Int): Boolean =
        h == 0x306F || h == 0x3072 || h == 0x3075 || h == 0x3078 || h == 0x307B

    /** The voiced form of kana [a]: the next code point, except for う and ワ ヰ ヱ ヲ. */
    private fun voiced(a: Int): Int? {
        val h = kanaRow(a)
        return when {
            h in 0x304B..0x3061 && h % 2 == 1 -> a + 1
            h == 0x3064 || h == 0x3066 || h == 0x3068 || h == 0x309D -> a + 1
            isHaRow(h) -> a + 1
            h == 0x3046 -> a + 0x4E
            h in 0x308F..0x3092 && a > 0x30A0 -> a + 8
            else -> null
        }
    }

    /** U+30A1-30F6 as the hiragana U+3041-3096. */
    private fun hiragana(c: Int): Int = if (c in 0x30A1..0x30F6) c - 0x60 else c

    /** Rust's `char::is_alphanumeric`: Alphabetic, or a number (Nd, Nl, No). */
    private fun isAlphanumeric(cp: Int): Boolean =
        Character.isAlphabetic(cp) || when (Character.getType(cp).toByte()) {
            Character.DECIMAL_DIGIT_NUMBER, Character.LETTER_NUMBER, Character.OTHER_NUMBER -> true
            else -> false
        }

    /** The Unicode White_Space property, which Rust's `char::is_whitespace` and `trim` use. */
    private fun isWhiteSpace(cp: Int): Boolean =
        cp in 0x09..0x0D || cp == 0x20 || cp == 0x85 || cp == 0xA0 || cp == 0x1680 ||
            cp in 0x2000..0x200A || cp == 0x2028 || cp == 0x2029 || cp == 0x202F ||
            cp == 0x205F || cp == 0x3000

    private fun String.trimWhiteSpace(): String =
        trim { it.code <= 0xFFFF && isWhiteSpace(it.code) }

    private fun String.codePointsArray(): IntArray {
        val out = ArrayList<Int>()
        forEachCodePoint { out += it }
        return out.toIntArray()
    }

    private inline fun String.forEachCodePoint(action: (Int) -> Unit) {
        var i = 0
        while (i < length) {
            val cp = codePointAt(i)
            action(cp)
            i += Character.charCount(cp)
        }
    }
}

/** One headed block of titles: [key] names the group, the screen words it. */
data class ShelfSection(val key: LibraryGroupKey, val games: List<GameEntry>)

/** The Library tab's title rows, from one pass over the catalog. */
data class ShelfRows(
    /** Played titles, newest first, at most [LibraryShelf.RECENT_LIMIT]. */
    val recent: List<GameEntry>,
    /** Favorite titles, in the shelf's sort. */
    val favorites: List<GameEntry>,
    /** The launcher entries, in the host's order. */
    val launchers: List<GameEntry>,
    /** The titles less their launchers, collated. */
    val games: List<ShelfSection>,
)

/** Which titles each row of the Library tab holds. */
object LibraryShelf {
    /** The Recent row's length. */
    const val RECENT_LIMIT = 12

    /**
     * The shelf's display order: the desktop tile, then launchers, running titles leading within
     * each band. A stable partition; the host's order survives inside each band.
     */
    fun display(games: List<GameEntry>, running: Set<String>): List<GameEntry> =
        games.sortedBy { g ->
            (if (g.isDesktop) 0 else 4) + (if (g.isLauncher) 0 else 2) + (if (g.id in running) 0 else 1)
        }

    /**
     * [games] collated: launchers lead, then one section per group under [groupBy] (none: one
     * section holding the titles), each in [sort]. Under [LibrarySortKey.HOST] a running title
     * leads its group; any other sort places it by its key.
     */
    fun grid(
        games: List<GameEntry>,
        sort: LibrarySortKey,
        groupBy: LibraryGroupBy?,
        running: Set<String>,
        storeLabel: (GameEntry) -> String,
    ): List<ShelfSection> {
        val ordered = display(games, running)
        return LibraryCollation.collate(ordered, sort, groupBy, storeLabel).map { group ->
            ShelfSection(group.key, group.indices.map { ordered[it] })
        }
    }

    /**
     * The tab's rows over the titles [search] leaves. Each row is filtered before it is sorted,
     * so a sort runs over its row and not the shelf.
     */
    fun rows(
        games: List<GameEntry>,
        sort: LibrarySortKey,
        groupBy: LibraryGroupBy?,
        running: Set<String>,
        favorites: Set<String>,
        search: String,
        storeLabel: (GameEntry) -> String,
    ): ShelfRows {
        val needle = LibraryCollation.searchFold(search)
        val shelf = if (search.isEmpty()) {
            games
        } else {
            games.filter { LibraryCollation.searchFold(it.title).contains(needle) }
        }
        val titles = shelf.filter { !it.isLauncher }
        val played = titles.filter { (it.stats?.lastPlayedUnixMs ?: 0) > 0 }
        val recent = LibraryCollation.collate(played, LibrarySortKey.RECENT, null, storeLabel)
            .firstOrNull()?.indices.orEmpty()
        val marked = shelf.filter { it.id in favorites }
        return ShelfRows(
            recent = recent.take(RECENT_LIMIT).map { played[it] },
            favorites = LibraryCollation.collate(marked, sort, null, storeLabel)
                .flatMap { it.indices }.map { marked[it] },
            launchers = shelf.filter { it.isLauncher },
            games = grid(titles, sort, groupBy, running, storeLabel),
        )
    }
}

/** One band of the Games tab, by the id the `library_sections` setting stores. */
enum class LibrarySection(val id: String) {
    DESKTOPS("desktops"),
    RECENT("recent"),
    FAVORITES("favorites"),
    LAUNCHERS("launchers"),
    COLLECTIONS("collections"),
    GAMES("games"),
}

object LibrarySections {
    /**
     * `library_sections` as every client parses it: ids in order, `-` before one switched off, an
     * unknown id dropped, a repeat keeping its first place, a known section the value lacks
     * appended switched on.
     */
    fun parse(stored: String?): List<Pair<LibrarySection, Boolean>> {
        val out = ArrayList<Pair<LibrarySection, Boolean>>()
        for (token in stored.orEmpty().split(',').map { it.trim() }) {
            val section = LibrarySection.entries.firstOrNull { it.id == token.removePrefix("-") }
                ?: continue
            if (out.none { it.first == section }) out += section to !token.startsWith('-')
        }
        for (section in LibrarySection.entries) {
            if (out.none { it.first == section }) out += section to true
        }
        return out
    }

    /** The stored form of [parse]'s result. */
    fun stored(sections: List<Pair<LibrarySection, Boolean>>): String =
        sections.joinToString(",") { (section, on) -> (if (on) "" else "-") + section.id }
}
