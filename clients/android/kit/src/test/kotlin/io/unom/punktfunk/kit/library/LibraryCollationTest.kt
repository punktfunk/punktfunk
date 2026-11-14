package io.unom.punktfunk.kit.library

import io.unom.punktfunk.kit.library.LibraryGroupKey.Launchers
import io.unom.punktfunk.kit.library.LibraryGroupKey.Platform
import io.unom.punktfunk.kit.library.LibraryGroupKey.Store
import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The library's sort and group rules against `clients/shared/library-collate-vectors.json`,
 * which the desktop and Apple ports replay too, plus the shelf rows and the section list.
 */
class LibraryCollationTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/kit).
        val file = File("../../shared/library-collate-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private val storeLabels: Map<String, String> by lazy {
        val table = vectors.getJSONObject("store_labels")
        table.keys().asSequence().associateWith { table.getString(it) }
    }

    private val label: (GameEntry) -> String = { storeLabels.getValue(it.store) }

    private fun JSONObject.optText(key: String): String? =
        if (has(key) && !isNull(key)) getString(key) else null

    private fun JSONArray.objects() = (0 until length()).map { getJSONObject(it) }

    private fun entry(
        id: String,
        title: String = id,
        store: String = "steam",
        platform: String? = null,
        role: String? = null,
        played: Long = 0,
        playTime: Long = 0,
    ) = GameEntry(
        id = id,
        store = store,
        title = title,
        art = Artwork(null, null, null),
        role = role,
        platform = platform,
        stats = if (played > 0 || playTime > 0) GameStats(lastPlayedUnixMs = played, playTimeMs = playTime) else null,
    )

    private fun launcher(id: String) = entry(id, role = "launcher")

    private fun ids(games: List<GameEntry>) = games.map { it.id }

    private val library: List<GameEntry> by lazy {
        vectors.getJSONArray("library").objects().map { o ->
            GameEntry(
                id = o.getString("id"),
                store = o.getString("store"),
                title = o.getString("title"),
                art = Artwork(null, null, null),
                role = o.optText("role"),
                icon = o.optText("icon"),
                platform = o.optText("platform"),
                stats = GameStats.from(o.optJSONObject("stats")),
            )
        }
    }

    private fun describe(key: LibraryGroupKey): Pair<String, String?> = when (key) {
        Launchers -> "launchers" to null
        is Platform -> "platform" to key.name
        is Store -> "store" to key.name
    }

    @Test
    fun theLibraryDecodesAsTheHostSendsIt() {
        assertEquals(3, vectors.getInt("version"))
        assertEquals(14, library.size)
        assertEquals("PS3", library.first { it.id == "custom:gow3" }.platform)
    }

    @Test
    fun sortTitlesMatchTheVectors() {
        for (c in vectors.getJSONArray("sort_title").objects()) {
            assertEquals("sortTitle(${c.getString("in")})", c.getString("out"), LibraryCollation.sortTitle(c.getString("in")))
        }
    }

    @Test
    fun titleOrderMatchesTheVectors() {
        for (c in vectors.getJSONArray("title_order").objects()) {
            val titles = c.getJSONArray("titles").let { t -> (0 until t.length()).map { t.getString(it) } }
            val shelf = titles.map { entry(it, store = "custom") }
            val got = LibraryCollation.collate(shelf, LibrarySortKey.TITLE, null, label)
                .flatMap { it.indices }.map { shelf[it].title }
            val want = c.getJSONArray("expect").let { e -> (0 until e.length()).map { e.getString(it) } }
            assertEquals(c.getString("name"), want, got)
        }
    }

    @Test
    fun sortKeysParseAsTheVectorsSay() {
        for (c in vectors.getJSONArray("sort_keys").objects()) {
            assertEquals(c.getString("stored"), c.getString("key"), LibrarySortKey.parse(c.getString("stored")).stored)
        }
        assertEquals(LibrarySortKey.HOST, LibrarySortKey.parse(null))
        assertEquals(LibraryGroupBy.STORE, LibraryGroupBy.parse("store"))
        assertNull(LibraryGroupBy.parse("host"))
        assertNull(LibraryGroupBy.parse(null))
    }

    @Test
    fun storeNamesAgreeWithTheLabelTable() {
        for ((store, label) in storeLabels) {
            assertEquals(store, label != "Game", entry("x", store = store).hasStoreName)
        }
    }

    @Test
    fun everyCollationCaseMatchesTheVectors() {
        val cases = vectors.getJSONArray("cases").objects()
        assertTrue("the vector file is the contract; keep it rich", cases.size >= 10)
        for (c in cases) {
            val name = c.getString("name")
            val got = LibraryCollation.collate(
                library, LibrarySortKey.parse(c.getString("sort")), LibraryGroupBy.parse(c.optText("group_by")), label,
            )
            val want = c.getJSONArray("expect").objects()
            assertEquals("$name: group count", want.size, got.size)
            for ((g, w) in got.zip(want)) {
                assertEquals("$name: group", w.getString("kind") to w.optText("name"), describe(g.key))
                val wantIds = w.getJSONArray("ids").let { a -> (0 until a.length()).map { a.getString(it) } }
                assertEquals("$name: ${describe(g.key)} ids", wantIds, g.indices.map { library[it].id })
            }
        }
    }

    @Test
    fun groupsSortByCodePointNotByUtf16Unit() {
        // U+FB01 is one UTF-16 unit above the surrogates; U+10428 is above it in UTF-8.
        val games = listOf(
            entry("astral", platform = "𐐨"),
            entry("bmp", platform = "ﬁ"),
        )
        val groups = LibraryCollation.collate(games, LibrarySortKey.HOST, LibraryGroupBy.PLATFORM, label)
        assertEquals(listOf("bmp", "astral"), groups.map { games[it.indices.single()].id })
    }

    @Test
    fun runningTitlesLeadWithoutBreakingTheLauncherPrefix() {
        val games = listOf(
            launcher("steam"), entry("heroic", store = "heroic", role = "launcher"),
            entry("a"), entry("b"), entry("c"),
        )
        assertEquals(listOf("heroic", "steam", "c", "a", "b"), ids(LibraryShelf.display(games, setOf("c", "heroic"))))
        assertEquals(ids(games), ids(LibraryShelf.display(games, emptySet())))
        assertEquals(listOf("steam", "heroic", "b", "a", "c"), ids(LibraryShelf.display(games, setOf("b"))))
        val withDesktop = games + GameEntry.desktop("Desktop")
        assertEquals(GameEntry.DESKTOP_ID, LibraryShelf.display(withDesktop, setOf("c")).first().id)
    }

    @Test
    fun theDesktopTileBelongsToNoSection() {
        val games = listOf(GameEntry.desktop("Desktop"), entry("a"))
        val sections = LibraryShelf.grid(games, LibrarySortKey.HOST, null, emptySet(), label)
        assertEquals(listOf(listOf("a")), sections.map { ids(it.games) })
        assertEquals(Platform(LibraryCollation.ALL), sections.single().key)
    }

    @Test
    fun recentHoldsTheTwelveNewestPlayedTitlesAndNoLaunchers() {
        val titles = (1..15).map { entry("g$it", played = it * 1000L) }
        val games = listOf(entry("never")) + titles + entry("l", role = "launcher", played = 99_000)
        val rows = LibraryShelf.rows(games, LibrarySortKey.HOST, null, emptySet(), emptySet(), "", label)
        assertEquals((15 downTo 4).map { "g$it" }, ids(rows.recent))
        assertEquals(LibraryShelf.RECENT_LIMIT, rows.recent.size)
    }

    @Test
    fun launchersStayOutOfTheGamesAndKeepHostOrder() {
        val games = listOf(entry("z"), launcher("second"), entry("a"), launcher("first"))
        val rows = LibraryShelf.rows(games, LibrarySortKey.TITLE, null, emptySet(), emptySet(), "", label)
        assertEquals(listOf("second", "first"), ids(rows.launchers))
        val section = rows.games.single()
        assertEquals(Platform(LibraryCollation.ALL), section.key)
        assertEquals(listOf("a", "z"), ids(section.games))
    }

    @Test
    fun favoritesFollowTheShelfsSort() {
        val games = listOf(entry("c", "Charlie"), entry("a", "Alpha"), entry("b", "Bravo"))
        val fav = setOf("c", "a")
        assertEquals(
            listOf("a", "c"),
            ids(LibraryShelf.rows(games, LibrarySortKey.TITLE, null, emptySet(), fav, "", label).favorites),
        )
        assertEquals(
            listOf("c", "a"),
            ids(LibraryShelf.rows(games, LibrarySortKey.HOST, null, emptySet(), fav, "", label).favorites),
        )
    }

    @Test
    fun searchFoldsWidthAndKana() {
        val games = listOf(
            entry("ff", "Final Fantasy"),
            entry("jp", "ファイナルファンタジー"),
            entry("halo", "Halo"),
            launcher("l"),
        )
        fun hits(q: String) = LibraryShelf.rows(games, LibrarySortKey.HOST, null, emptySet(), emptySet(), q, label)
            .games.flatMap { ids(it.games) }
        assertEquals(listOf("ff"), hits("ＦＩＮＡＬ"))
        assertEquals(listOf("jp"), hits("ﾌｧｲﾅﾙ"))
        assertEquals(listOf("jp"), hits("ふぁいなる"))
        assertEquals(listOf("ff", "jp", "halo"), hits(""))
        assertTrue(hits("zzz").isEmpty())
        val none = LibraryShelf.rows(games, LibrarySortKey.HOST, null, emptySet(), emptySet(), "zzz", label)
        assertTrue(none.launchers.isEmpty())
    }

    @Test
    fun sectionsParseAsEveryClientDoes() {
        assertEquals(LibrarySection.entries.map { it to true }, LibrarySections.parse(""))
        assertEquals(LibrarySection.entries.map { it to true }, LibrarySections.parse(null))
        assertEquals(
            listOf(
                LibrarySection.GAMES to true,
                LibrarySection.RECENT to false,
                LibrarySection.DESKTOPS to true,
                LibrarySection.FAVORITES to true,
                LibrarySection.LAUNCHERS to true,
                LibrarySection.COLLECTIONS to true,
            ),
            LibrarySections.parse("games,-recent,bogus,games,desktops"),
        )
        val stored = "desktops,recent,-favorites,launchers,-collections,games"
        assertEquals(stored, LibrarySections.stored(LibrarySections.parse(stored)))
        assertEquals(LibrarySection.entries.map { it to true }, LibrarySections.parse("--games"))
    }
}
