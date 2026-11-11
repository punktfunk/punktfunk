import Foundation
import XCTest

@testable import PunktfunkKit

/// The library's sort/group rules, against the desktop's `collate.rs` — by name (each case here
/// is one of that module's tests) and by the shared vectors file
/// (`clients/shared/library-collate-vectors.json`, which the desktop test reads too). If the
/// vectors test goes red after a desktop rule change, the rule moved and this port owes the
/// same change; if it goes red after an edit here, the port drifted.
final class LibraryCollationTests: XCTestCase {
    /// Four `deletingLastPathComponent()` calls walk `Tests/PunktfunkKitTests/` → `Tests/` →
    /// `apple/` → `clients/`, the same way `StartScreenVectorsTests` finds its file.
    private static var vectorFileURL: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("shared/library-collate-vectors.json")
    }

    private func entry(
        _ id: String, _ title: String, store: String = "steam", platform: String? = nil,
        launcher: Bool = false
    ) -> GameEntry {
        var json: [String: Any] = ["id": id, "store": store, "title": title, "art": [:]]
        if let platform { json["platform"] = platform }
        if launcher { json["role"] = "launcher" }
        let data = try! JSONSerialization.data(withJSONObject: json)
        return try! JSONDecoder().decode(GameEntry.self, from: data)
    }

    private func ids(_ games: [GameEntry], _ indices: [Int]) -> [String] {
        indices.map { games[$0].id }
    }

    // MARK: - The desktop's tests, by name

    func testArticleFoldFilesTitlesWhereAReaderLooksForThem() {
        XCTAssertEqual(LibraryCollation.sortTitle("The Witcher 3"), "witcher 3")
        XCTAssertEqual(LibraryCollation.sortTitle("A Way Out"), "way out")
        XCTAssertEqual(LibraryCollation.sortTitle("An Untitled Story"), "untitled story")
        // Not an article, just a word starting with one.
        XCTAssertEqual(LibraryCollation.sortTitle("Theme Hospital"), "theme hospital")
        XCTAssertEqual(LibraryCollation.sortTitle("Anno 1800"), "anno 1800")
        // Diacritics relax; punctuation goes.
        XCTAssertEqual(LibraryCollation.sortTitle("Pokémon: Red!"), "pokemon red")
        // A title that is ONLY an article keeps something to sort on.
        XCTAssertEqual(LibraryCollation.sortTitle("The"), "the")
    }

    func testPlatformLessStoreGamesBucketUnderTheirStoreNotUnknown() {
        let games = [
            entry("a", "Dota 2"),
            entry("b", "Half-Life"),
            entry("c", "Shadow of the Colossus", store: "custom", platform: "PS2"),
            // Neither a platform nor a store we have a label for.
            entry("d", "Mystery", store: "weird"),
            // A whitespace-only platform is no platform.
            entry("e", "Blank", store: "gog", platform: "  "),
        ]
        let groups = LibraryCollation.collate(games, sort: .hostOrder, groupBy: .platform)
        XCTAssertEqual(groups.map(\.label), ["GOG", "Other", "PS2", "Steam"])
        XCTAssertEqual(groups.map(\.key), [.store("GOG"), .platform("Other"), .platform("PS2"), .store("Steam")])
        XCTAssertEqual(ids(games, groups[3].indices), ["a", "b"])
        XCTAssertEqual(ids(games, groups[1].indices), ["d"])
        XCTAssertFalse(groups.contains { $0.label == "Unknown" })
    }

    func testLaunchersAlwaysLeadWhateverTheSort() {
        let games = [
            entry("z", "Zebra"),
            entry("steam", "Steam", launcher: true),
            entry("a", "Aardvark", store: "gog"),
            entry("heroic", "Heroic", store: "heroic", launcher: true),
        ]
        for sort in LibrarySortKey.all {
            for by in [nil, LibraryGroupBy.platform, .store] {
                let groups = LibraryCollation.collate(games, sort: sort, groupBy: by)
                XCTAssertEqual(groups.first?.key, .launchers, "\(sort) / \(String(describing: by))")
                // Host order inside the launcher group, whatever the sort.
                XCTAssertEqual(ids(games, groups[0].indices), ["steam", "heroic"], "\(sort)")
            }
        }
    }

    func testHostOrderIsByteIdenticalToNoSortingAtAll() {
        let games = [
            entry("c", "Charlie"), entry("a", "Alpha"), entry("b", "Bravo", store: "gog"),
        ]
        let groups = LibraryCollation.collate(games, sort: .hostOrder, groupBy: nil)
        XCTAssertEqual(groups.count, 1)
        XCTAssertEqual(groups[0].label, "All")
        XCTAssertEqual(ids(games, groups[0].indices), ["c", "a", "b"])
        XCTAssertEqual(
            LibraryCollation.filtered(games, sort: .hostOrder, filter: nil), [0, 1, 2])
    }

    func testEqualKeysKeepTheHostsOrder() {
        let games = [
            entry("second", "Same Name", store: "custom"),
            entry("first", "Same Name", store: "custom"),
            entry("third", "same name!", store: "custom"),
        ]
        for sort in [LibrarySortKey.title, .platform, .store] {
            let groups = LibraryCollation.collate(games, sort: sort, groupBy: nil)
            XCTAssertEqual(ids(games, groups[0].indices), ["second", "first", "third"], "\(sort)")
        }
    }

    func testFilteringReturnsOnlyThatGroupsGames() {
        let games = [
            entry("l", "Steam", launcher: true),
            entry("p1", "Portal", platform: "PC"),
            entry("g1", "God of War", store: "custom", platform: "PS3"),
            entry("g2", "Ico", store: "custom", platform: "PS3"),
            entry("s1", "Half-Life"),
        ]
        XCTAssertEqual(
            ids(games, LibraryCollation.filtered(games, sort: .hostOrder, filter: .platform("PS3"))),
            ["g1", "g2"])
        XCTAssertEqual(
            ids(games, LibraryCollation.filtered(games, sort: .title, filter: .store("Steam"))),
            ["s1", "p1"])
        XCTAssertEqual(
            ids(games, LibraryCollation.filtered(games, sort: .hostOrder, filter: .launchers)), ["l"])
        // A group that isn't there yields nothing — never everything.
        XCTAssertEqual(LibraryCollation.filtered(games, sort: .hostOrder, filter: .platform("SNES")), [])
    }

    func testEmptyAndSingleGroupLibrariesAreNotWorthBrowsing() {
        XCTAssertFalse(LibraryCollation.worthBrowsing([]))
        XCTAssertFalse(LibraryCollation.worthBrowsing([entry("l", "Steam", launcher: true)]))
        XCTAssertFalse(LibraryCollation.worthBrowsing([entry("a", "A"), entry("b", "B")]))
        XCTAssertFalse(LibraryCollation.worthBrowsing([
            entry("l", "Steam", launcher: true), entry("a", "A"), entry("b", "B"),
        ]))
        XCTAssertTrue(LibraryCollation.worthBrowsing([entry("a", "A"), entry("b", "B", store: "gog")]))
        XCTAssertTrue(LibraryCollation.worthBrowsing([
            entry("a", "A", store: "custom", platform: "PS3"), entry("b", "B"),
        ]))
    }

    func testSortKeysParseFromTheirStoredNamesAndUnknownFallsBack() {
        XCTAssertEqual(LibrarySortKey(stored: "title"), .title)
        XCTAssertEqual(LibrarySortKey(stored: "platform"), .platform)
        XCTAssertEqual(LibrarySortKey(stored: "store"), .store)
        for key in LibrarySortKey.all {
            XCTAssertEqual(LibrarySortKey(stored: key.stored), key, "\(key.label) round-trips")
        }
        XCTAssertEqual(LibrarySortKey(stored: "something-newer"), .hostOrder)
        XCTAssertEqual(LibrarySortKey(stored: ""), .hostOrder)
        XCTAssertEqual(LibrarySortKey(stored: nil), .hostOrder)
        XCTAssertEqual(LibrarySortKey.all.map(\.stored), ["host", "title", "platform", "store", "recent", "playtime"])

        XCTAssertEqual(LibraryArrangement(stored: "grid"), .grid)
        XCTAssertEqual(LibraryArrangement(stored: "shelf"), .shelf)
        // The console reads unset and unknown as the grid (`LibraryView::parse`), and draws it.
        XCTAssertEqual(LibraryArrangement(stored: "carousel"), .grid)
        XCTAssertEqual(LibraryArrangement(stored: ""), .grid)
        XCTAssertEqual(LibraryArrangement(stored: nil), .grid)
    }

    /// The desktop model's `running_titles_lead_without_breaking_the_launcher_prefix`: launchers
    /// lead absolutely, running titles lead within their band, host order inside each band.
    func testRunningTitlesLeadWithoutBreakingTheLauncherPrefix() {
        let games = [
            entry("steam", "Steam", launcher: true),
            entry("heroic", "Heroic", store: "heroic", launcher: true),
            entry("a", "Alpha"),
            entry("b", "Bravo"),
            entry("c", "Charlie"),
        ]
        let order = LibraryOrder.display(games, running: ["c", "heroic"]).map(\.id)
        XCTAssertEqual(order, ["heroic", "steam", "c", "a", "b"])
        // Nothing running: byte-identical.
        XCTAssertEqual(LibraryOrder.display(games, running: []).map(\.id), games.map(\.id))
        // A running GAME never jumps ahead of a launcher — the bug this replaced.
        XCTAssertEqual(
            LibraryOrder.display(games, running: ["b"]).map(\.id), ["steam", "heroic", "b", "a", "c"])
    }

    // MARK: - The shared vectors file

    private struct VectorFile: Decodable {
        struct TitleCase: Decodable { let `in`: String; let out: String }
        struct SortKeyCase: Decodable { let stored: String; let key: String }
        struct GroupExpect: Decodable {
            let kind: String
            let name: String?
            let label: String
            let ids: [String]
        }
        struct Case: Decodable {
            let name: String
            let sort: String
            let groupBy: String?
            let expect: [GroupExpect]
            enum CodingKeys: String, CodingKey { case name, sort, groupBy = "group_by", expect }
        }
        struct FilterKey: Decodable { let kind: String; let name: String? }
        struct FilteredCase: Decodable {
            let name: String
            let sort: String
            let filter: FilterKey?
            let expect: [String]
        }
        struct BrowsableCase: Decodable { let name: String; let ids: [String]?; let expect: Bool }
        struct OrderCase: Decodable { let name: String; let titles: [String]; let expect: [String] }

        let version: Int
        let library: [GameEntry]
        let sortTitle: [TitleCase]
        let titleOrder: [OrderCase]
        let storeLabels: [String: String]
        let sortKeys: [SortKeyCase]
        let cases: [Case]
        let filtered: [FilteredCase]
        let worthBrowsing: [BrowsableCase]

        enum CodingKeys: String, CodingKey {
            case version, library, cases, filtered
            case sortTitle = "sort_title"
            case titleOrder = "title_order"
            case storeLabels = "store_labels"
            case sortKeys = "sort_keys"
            case worthBrowsing = "worth_browsing"
        }
    }

    private func key(kind: String, name: String?) -> LibraryGroupKey {
        switch kind {
        case "launchers": return .launchers
        case "platform": return .platform(name!)
        case "store": return .store(name!)
        default: XCTFail("unknown group kind \(kind)"); return .launchers
        }
    }

    private func groupBy(_ raw: String?) -> LibraryGroupBy? {
        switch raw {
        case nil: return nil
        case "platform": return .platform
        case "store": return .store
        default: XCTFail("unknown group_by \(raw!)"); return nil
        }
    }

    func testVectorsMatchTheSharedFile() throws {
        let data = try Data(contentsOf: Self.vectorFileURL)
        let file = try JSONDecoder().decode(VectorFile.self, from: data)
        XCTAssertEqual(file.version, 3, "bump the reader when the file's version moves")
        let games = file.library
        // The library array is the host wire shape, so the real model decodes it — including
        // `platform`, which is the field this whole feature hangs on.
        XCTAssertEqual(games.count, 14)
        XCTAssertEqual(games.first { $0.id == "custom:gow3" }?.platform, "PS3")

        for c in file.sortTitle {
            // Scalars, not `String ==`: Swift calls か + U+3099 and が equal.
            XCTAssertEqual(
                Array(LibraryCollation.sortTitle(c.in).unicodeScalars), Array(c.out.unicodeScalars),
                "sortTitle(\(c.in))")
        }
        for c in file.titleOrder {
            let shelf = c.titles.map { entry($0, $0, store: "custom") }
            let got = LibraryCollation.filtered(shelf, sort: .title, filter: nil).map { shelf[$0].title }
            XCTAssertEqual(got.map { Array($0.unicodeScalars) }, c.expect.map { Array($0.unicodeScalars) }, c.name)
        }
        for (store, label) in file.storeLabels {
            XCTAssertEqual(entry("x", "X", store: store).storeLabel, label, "storeLabel(\(store))")
        }
        for c in file.sortKeys {
            XCTAssertEqual(LibrarySortKey(stored: c.stored).stored, c.key, "parse(\(c.stored))")
        }
        for c in file.cases {
            let got = LibraryCollation.collate(games, sort: LibrarySortKey(stored: c.sort), groupBy: groupBy(c.groupBy))
            XCTAssertEqual(got.count, c.expect.count, "\(c.name): group count")
            for (g, w) in zip(got, c.expect) {
                XCTAssertEqual(g.key, key(kind: w.kind, name: w.name), "\(c.name): group key")
                XCTAssertEqual(g.label, w.label, "\(c.name): label")
                XCTAssertEqual(ids(games, g.indices), w.ids, "\(c.name): \(g.label) ids")
            }
        }
        for c in file.filtered {
            let filter = c.filter.map { key(kind: $0.kind, name: $0.name) }
            XCTAssertEqual(
                ids(games, LibraryCollation.filtered(games, sort: LibrarySortKey(stored: c.sort), filter: filter)),
                c.expect, c.name)
        }
        for c in file.worthBrowsing {
            let subset: [GameEntry]
            if let want = c.ids {
                subset = want.map { id in games.first { $0.id == id }! }
            } else {
                subset = games
            }
            XCTAssertEqual(LibraryCollation.worthBrowsing(subset), c.expect, c.name)
        }
    }
}
