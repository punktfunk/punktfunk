// The touch shelf's rows, its load reducer and a title's acts (`LibraryShelf`).

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

    private func running(_ id: String, state: String = "running", endable: Bool? = nil) -> RunningGame {
        var json: [String: Any] = ["app_id": id, "title": id, "state": state]
        if let endable { json["endable"] = endable }
        let data = try! JSONSerialization.data(withJSONObject: json)
        return try! JSONDecoder().decode(RunningGame.self, from: data)
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

    // MARK: - The load reducer

    private func reduce(
        _ state: LibraryShelf.State, _ events: [LibraryLoadEvent], autoWake: Bool = true,
        canWake: Bool = true
    ) -> LibraryShelf.State {
        events.reduce(state) { LibraryShelf.reduce($0, $1, autoWake: autoWake, canWake: canWake) }
    }

    private var loading: LibraryShelf.State {
        var state = LibraryShelf.State()
        state.loading = true
        return state
    }

    func testACachedShelfIsRememberedUntilTheHostAnswers() {
        let cached = CachedLibrary(games: [entry("a", "Alpha")], fetchedAt: Date(timeIntervalSince1970: 9))
        let waking = reduce(loading, [.cached(cached), .waking])
        XCTAssertEqual(ids(waking.games), ["a"])
        XCTAssertEqual(waking.servedFromCacheAt, Date(timeIntervalSince1970: 9))
        XCTAssertEqual(waking.staleness, .waking)

        let fetched = reduce(waking, [.fetched([entry("b", "Bravo")]), .status(games: [], downloads: [], grants: nil)])
        XCTAssertEqual(ids(fetched.games), ["b"])
        XCTAssertEqual(fetched.staleness, .none)
        XCTAssertFalse(fetched.loading)
    }

    func testAHostThatNeverAnswersLeavesTheCacheOffline() {
        let cached = CachedLibrary(games: [entry("a", "Alpha")], fetchedAt: Date())
        let state = reduce(loading, [
            .cached(cached), .failed(.unreachable("timed out"), hadCache: true),
            .status(games: [], downloads: [], grants: nil),
        ])
        XCTAssertEqual(ids(state.games), ["a"])
        XCTAssertNil(state.errorText)
        XCTAssertEqual(state.staleness, .offline)
    }

    func testAFailureOnAnEmptyShelfSaysWhyAndAFetchClearsIt() {
        let failed = reduce(loading, [.failed(.unauthorized, hadCache: false)])
        XCTAssertEqual(failed.errorText, LibraryLoad.failure(.unauthorized))
        XCTAssertNil(reduce(failed, [.fetched([entry("a", "Alpha")])]).errorText)
    }

    func testOnlyAnUnreachableHostWithAutoWakeOffIsOfferedAWake() {
        let down = LibraryLoadEvent.failed(.unreachable("refused"), hadCache: false)
        XCTAssertTrue(reduce(loading, [down], autoWake: false, canWake: true).wakeOffered)
        XCTAssertFalse(reduce(loading, [down], autoWake: true, canWake: true).wakeOffered)
        XCTAssertFalse(reduce(loading, [down], autoWake: false, canWake: false).wakeOffered)
        let refused = LibraryLoadEvent.failed(.unauthorized, hadCache: false)
        XCTAssertFalse(reduce(loading, [refused], autoWake: false, canWake: true).wakeOffered)
    }

    func testTheStatusEndsTheLoadAndKeysWhatRuns() {
        let state = reduce(loading, [.status(
            games: [
                running("a"), running("a", endable: true), running("b", state: "exited"),
                running("c", state: "grace"),
            ],
            downloads: [HostDownload(appID: "d", state: "downloading")], grants: 5)])
        XCTAssertFalse(state.loading)
        XCTAssertEqual(Set(state.running.keys), ["a", "c"])
        XCTAssertEqual(state.running["a"]?.endable, true)
        XCTAssertEqual(Array(state.downloads.keys), ["d"])
        XCTAssertEqual(state.grants, 5)
    }

    func testALiveDownloadKeepsThePollGoing() {
        let live = LibraryShelf.State().applying(
            ([], [HostDownload(appID: "d", state: "downloading")], nil))
        XCTAssertTrue(live.polls)
        let paused = LibraryShelf.State().applying(
            ([], [HostDownload(appID: "d", state: "paused")], nil))
        XCTAssertFalse(paused.polls)
    }

    /// R5-1: one missed read keeps the badges, the grants and the poll.
    func testAMissedReadKeepsTheLastAnswer() {
        let live = LibraryShelf.State().applying(
            ([running("a")], [HostDownload(appID: "d", state: "downloading")], 3))
        let missed = live.applying(nil)
        XCTAssertEqual(missed, live)
        XCTAssertTrue(missed.polls)
        XCTAssertEqual(Set(missed.running.keys), ["a"])
        XCTAssertEqual(missed.grants, 3)
        // An answer that nothing runs is still an answer.
        XCTAssertFalse(live.applying(([], [], nil)).polls)
    }

    // MARK: - A title's acts

    private func acts(
        _ game: GameEntry, running: RunningGame? = nil, download: HostDownload? = nil,
        grants: UInt32? = nil, canLaunch: Bool = true, favorite: Bool? = nil,
        canCopyLink: Bool = true
    ) -> [LibraryShelf.Act] {
        LibraryShelf.acts(
            game, running: running, download: download, grants: grants, canLaunch: canLaunch,
            favorite: favorite, canCopyLink: canCopyLink)
    }

    func testATitleOffersPlayThenWhatATapCannotDo() {
        let game = entry("a", "Alpha")
        XCTAssertEqual(acts(game), [.play, .details, .copyLink])
        XCTAssertEqual(acts(game, favorite: false), [.play, .favorite, .details, .copyLink])
        XCTAssertEqual(acts(game, favorite: true, canCopyLink: false), [.play, .unfavorite, .details])
        XCTAssertEqual(acts(game, canLaunch: false), [.details, .copyLink])
    }

    func testARunningTitleResumesAndOnlyThisDevicesLaunchEnds() {
        let game = entry("a", "Alpha")
        XCTAssertEqual(acts(game, running: running("a")), [.resume, .details, .copyLink])
        XCTAssertEqual(
            acts(game, running: running("a", endable: true)),
            [.resume, .details, .copyLink, .endGame])
    }

    func testTheDesktopTileConnectsAndCarriesNoTitleActs() {
        let desktop = LibraryCollation.desktopEntry()
        XCTAssertEqual(
            acts(desktop, running: running(LibraryCollation.desktopID, endable: true), favorite: true),
            [.desktop, .copyLink])
    }

    func testTheFilesActFollowsTheGrants() {
        let missing = entry("a", "Alpha", missing: true)
        XCTAssertEqual(acts(missing).last, .files(.install))
        XCTAssertEqual(acts(missing, grants: 0), [.play, .details, .copyLink])
        let downloading = HostDownload(appID: "a", state: "downloading")
        XCTAssertEqual(
            acts(missing, download: downloading, grants: PunktfunkConnection.grantManageGames).last,
            .files(.pause))
    }

    func testPlayWaitsForTheDetailsSheetToClose() {
        for act: LibraryShelf.Act in [.play, .resume, .desktop] {
            XCTAssertTrue(act.waitsForDetailsToClose, "\(act)")
        }
        for act: LibraryShelf.Act in [.favorite, .unfavorite, .details, .copyLink, .files(.install)] {
            XCTAssertFalse(act.waitsForDetailsToClose, "\(act)")
        }
    }

    /// R5-7, as it stands: End Game from the sheet presents at once.
    func testEndGameDoesNotWaitForTheDetailsSheet() {
        XCTAssertFalse(LibraryShelf.Act.endGame.waitsForDetailsToClose)
    }
}
