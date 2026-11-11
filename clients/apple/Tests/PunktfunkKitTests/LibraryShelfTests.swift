// The touch shelf's rows (`LibraryShelf`).

import XCTest
@testable import PunktfunkKit

final class LibraryShelfTests: XCTestCase {
    private func entry(
        _ id: String, _ title: String, launcher: Bool = false, platform: String? = nil,
        played: UInt64? = nil, missing: Bool = false
    ) -> GameEntry {
        var json: [String: Any] = ["id": id, "store": "steam", "title": title, "art": [:]]
        if launcher { json["role"] = "launcher" }
        if let platform { json["platform"] = platform }
        if let played { json["stats"] = ["last_played_unix_ms": played] }
        if missing { json["install"] = ["state": "missing"] }
        let data = try! JSONSerialization.data(withJSONObject: json)
        return try! JSONDecoder().decode(GameEntry.self, from: data)
    }

    private var gamesLabel: String { String(localized: L10n.libSectionGames) }

    private func ids(_ games: [GameEntry]) -> [String] { games.map(\.id) }

    private func rows(
        _ games: [GameEntry], sort: LibrarySortKey = .hostOrder, groupBy: LibraryGroupBy? = nil,
        running: Set<String> = [], favorites: Set<String> = [], search: String = ""
    ) -> LibraryShelf.Rows {
        LibraryShelf.rows(
            games, sort: sort, groupBy: groupBy, running: running, favorites: favorites,
            search: search)
    }

    // MARK: - Rows

    func testRecentHoldsPlayedTitlesNewestFirstAtMostTwelve() {
        var games = (1...14).map { entry("g\($0)", "Game \($0)", played: UInt64($0)) }
        games.append(entry("never", "Never Played"))
        games.append(entry("steam", "Steam", launcher: true, played: 99))
        let recent = rows(games).recent
        XCTAssertEqual(ids(recent), (3...14).reversed().map { "g\($0)" })
    }

    func testFavoritesFollowTheShelfSortLaunchersFirst() {
        let games = [
            entry("z", "Zebra"), entry("a", "Aardvark"), entry("m", "Mole"),
            entry("steam", "Steam", launcher: true),
        ]
        let marked: Set = ["z", "a", "steam"]
        XCTAssertEqual(ids(rows(games, sort: .title, favorites: marked).favorites), ["steam", "a", "z"])
        XCTAssertEqual(ids(rows(games, favorites: marked).favorites), ["steam", "z", "a"])
        XCTAssertTrue(rows(games).favorites.isEmpty)
    }

    func testLaunchersHaveTheirOwnRowInHostOrder() {
        let games = [
            entry("heroic", "Heroic", launcher: true), entry("a", "Alpha"),
            entry("steam", "Steam", launcher: true),
        ]
        let shelf = rows(games, sort: .title)
        XCTAssertEqual(ids(shelf.launchers), ["heroic", "steam"])
        XCTAssertEqual(shelf.games.map(\.label), [gamesLabel])
        XCTAssertEqual(ids(shelf.games[0].games), ["a"])
    }

    func testRunningTitlesLeadTheGamesSection() {
        let games = [entry("a", "Alpha"), entry("b", "Bravo"), entry("c", "Charlie")]
        XCTAssertEqual(ids(rows(games, running: ["c"]).games[0].games), ["c", "a", "b"])
    }

    func testGroupedGamesWearTheirGroupLabels() {
        let games = [entry("a", "Alpha", platform: "SNES"), entry("b", "Bravo", platform: "PC")]
        let sections = rows(games, groupBy: .platform).games
        XCTAssertEqual(sections.map(\.label), ["PC", "SNES"])
        XCTAssertEqual(sections.map { ids($0.games) }, [["b"], ["a"]])
    }

    func testSearchNarrowsEveryRow() {
        let games = [
            entry("a", "Hollow Knight", played: 5), entry("b", "Celeste", played: 6),
            entry("steam", "Steam", launcher: true), entry("hk", "Knightfall Launcher", launcher: true),
        ]
        let shelf = rows(games, favorites: ["a", "b", "hk"], search: "knight")
        XCTAssertEqual(ids(shelf.recent), ["a"])
        XCTAssertEqual(ids(shelf.favorites), ["hk", "a"])
        XCTAssertEqual(ids(shelf.launchers), ["hk"])
        XCTAssertEqual(shelf.games.map { ids($0.games) }, [["a"]])
    }

    func testTheGridKeepsLaunchersAndNeverTheDesktopTile() {
        let games = [
            LibraryCollation.desktopEntry(), entry("a", "Alpha"), entry("b", "Bravo"),
            entry("steam", "Steam", launcher: true),
        ]
        let sections = LibraryShelf.grid(games, sort: .hostOrder, groupBy: nil, running: ["b"])
        XCTAssertEqual(
            sections.map(\.label), [String(localized: L10n.libSectionLaunchers), gamesLabel])
        XCTAssertEqual(sections.map { ids($0.games) }, [["steam"], ["b", "a"]])
    }

    func testTheStoredGroupingParses() {
        XCTAssertEqual(LibraryGroupBy(stored: "platform"), .platform)
        XCTAssertEqual(LibraryGroupBy(stored: "store"), .store)
        XCTAssertNil(LibraryGroupBy(stored: ""))
        XCTAssertNil(LibraryGroupBy(stored: "genre"))
    }
}
