// "Hide for me" (`HideForMe`): `clients/shared/hide-for-me-vectors.json`, the rule Rust and Kotlin
// replay too, plus the shelf's entry, its focus handoff and the cached catalog.

import XCTest
@testable import PunktfunkKit

final class HideForMeTests: XCTestCase {
    private func json(_ value: Any?) throws -> Data {
        guard let value, !(value is NSNull) else { return Data() }
        return try JSONSerialization.data(withJSONObject: value, options: .fragmentsAllowed)
    }

    func testTheOfferFollowsTheSharedVectors() throws {
        let cases = try XCTUnwrap(SharedVector.load("hide-for-me-vectors.json")["offer"] as? [[String: Any]])
        XCTAssertFalse(cases.isEmpty)
        for c in cases {
            let name = c["name"] as? String ?? ""
            let listed = try (c["listed"] as? [Any]).map {
                try JSONDecoder().decode([ListedProfile].self, from: json($0))
            }
            let row = HideForMe.offer(listed: listed, pick: c["pick"] as? String)
            XCTAssertEqual(row?.displayName, c["offered"] as? String, name)
        }
    }

    /// The library's "Hidden for <name>" button exists exactly where the offer does: a host or
    /// profile without the flag keeps today's toolbar.
    func testTheHiddenButtonFollowsTheOffer() throws {
        let cases = try XCTUnwrap(SharedVector.load("hide-for-me-vectors.json")["offer"] as? [[String: Any]])
        for c in cases {
            let listed = try (c["listed"] as? [Any]).map {
                try JSONDecoder().decode([ListedProfile].self, from: json($0))
            }
            let button = HideForMe.hiddenEntry(HideForMe.offer(listed: listed, pick: c["pick"] as? String))
            let want = (c["offered"] as? String).map { "Hidden for \($0)" }
            XCTAssertEqual(button, want, c["name"] as? String ?? "")
        }
    }

    func testAHideFollowsTheSharedVectors() throws {
        let cases = try XCTUnwrap(
            SharedVector.load("hide-for-me-vectors.json")["set_hidden"] as? [[String: Any]])
        XCTAssertFalse(cases.isEmpty)
        for c in cases {
            let name = c["name"] as? String ?? ""
            let (path, body) = HideForMe.request(
                profile: c["profile"] as? String ?? "", id: c["id"] as? String ?? "",
                hidden: c["hidden"] as? Bool ?? false)
            XCTAssertEqual(path, c["path"] as? String, name)
            let sent = try JSONSerialization.jsonObject(with: body) as? NSDictionary
            XCTAssertEqual(sent, c["body"] as? NSDictionary, name)
            let reply = try XCTUnwrap(c["reply"] as? [String: Any], name)
            let outcome = HideForMe.Outcome.from(
                status: reply["status"] as? Int ?? 0, body: try json(reply["body"]))
            let (kind, said): (String, String?) = switch outcome {
            case .done: ("done", nil)
            case .refused(let s): ("refused", s)
            case .failed: ("failed", nil)
            }
            XCTAssertEqual(kind, c["outcome"] as? String, name)
            XCTAssertEqual(said, c["said"] as? String, name)
        }
    }

    func testTheHiddenListFollowsTheSharedVectors() async throws {
        let cases = try XCTUnwrap(
            SharedVector.load("hide-for-me-vectors.json")["hidden_list"] as? [[String: Any]])
        XCTAssertFalse(cases.isEmpty)
        for c in cases {
            let name = c["name"] as? String ?? ""
            let host = c["host"] as? [String: [String: Any]] ?? [:]
            var asked: [String] = []
            let shown = try? await HideForMe.fetchHidden(profile: c["profile"] as? String ?? "") {
                asked.append($0)
                guard let answer = host[$0] else {
                    return HTTPResponse(status: 404, headers: [:], body: Data())
                }
                return HTTPResponse(
                    status: answer["status"] as? Int ?? 0, headers: [:],
                    body: try self.json(answer["body"]))
            }
            XCTAssertEqual(asked, c["asked"] as? [String], "\(name): asked")
            XCTAssertEqual(shown?.map(\.id), c["ids"] as? [String], "\(name): ids")
        }
    }

    /// Every outcome has a line, and the host's sentence stands as it came.
    func testEachOutcomeReadsAsOneLine() {
        let refused = HideForMe.Outcome.refused("This device can't change this profile.")
        XCTAssertEqual(
            refused.notice(hidden: true, title: "Dota 2", name: "Kid"),
            "This device can't change this profile.")
        XCTAssertEqual(
            HideForMe.Outcome.done.notice(hidden: true, title: "Dota 2", name: "Kid"),
            "Dota 2 is hidden for Kid")
        XCTAssertEqual(
            HideForMe.Outcome.done.notice(hidden: false, title: "Dota 2", name: "Kid"),
            "Dota 2 is back for Kid")
        XCTAssertEqual(
            HideForMe.Outcome.failed("no answer").notice(hidden: false, title: "Dota 2", name: "Kid"),
            "Couldn't show Dota 2 — no answer")
    }

    /// A row with no `person` is an old host's: it reads false and crosses the console bridge
    /// without the key, as before. One that says so carries it across.
    func testAnOldHostsRowCrossesTheBridgeUnchanged() throws {
        let old = #"{"id": "a1", "display_name": "Kid", "owner": false, "unlock": "none"}"#
        let row = try JSONDecoder().decode(ListedProfile.self, from: Data(old.utf8))
        XCTAssertFalse(row.person)
        let out = try JSONSerialization.jsonObject(with: JSONEncoder().encode(row)) as? [String: Any]
        XCTAssertNil(out?["person"])
        XCTAssertEqual(out?["display_name"] as? String, "Kid")

        let marked = ListedProfile(id: "a1", displayName: "Kid", person: true)
        let back = try JSONSerialization.jsonObject(with: JSONEncoder().encode(marked)) as? [String: Any]
        XCTAssertEqual(back?["person"] as? Bool, true)
    }

    private func game(_ id: String) -> GameEntry {
        let row: [String: Any] = ["id": id, "store": "steam", "title": id, "art": [:]]
        return try! JSONDecoder().decode(
            GameEntry.self, from: JSONSerialization.data(withJSONObject: row))
    }

    /// The entry follows Favorite, only where a profile is named, never on the desktop tile; with
    /// none the menu is today's.
    func testTheTitleMenuOffersHideOnlyWhereTheHostSaysSo() {
        func acts(_ id: String, _ hides: String?) -> [LibraryShelf.Act] {
            LibraryShelf.acts(
                game(id), running: nil, download: nil, grants: nil, canLaunch: true,
                favorite: false, canCopyLink: true, hides: hides)
        }
        XCTAssertEqual(acts("steam:570", nil), [.play, .favorite, .details, .copyLink])
        XCTAssertEqual(acts("steam:570", "Kid"), [.play, .favorite, .hide("Kid"), .details, .copyLink])
        XCTAssertEqual(LibraryShelf.Act.hide("Kid").label(nil), "Hide for Kid")
        XCTAssertFalse(acts(LibraryCollation.desktopID, "Kid").contains(.hide("Kid")))
    }

    /// Focus takes the next tile in the row, else the one before, else a row below, else one
    /// above, skipping every copy of the title that leaves.
    func testANeighbourTakesTheFocusOfATitleThatLeaves() {
        let rows = [["recent:b", "recent:c"], ["favorites:b"], ["a", "b", "c"]]
        XCTAssertEqual(LibraryShelf.neighbour(of: "b", in: rows), "c")
        XCTAssertEqual(LibraryShelf.neighbour(of: "c", in: rows), "b")
        let copies: Set = ["b", "recent:b", "favorites:b"]
        XCTAssertEqual(LibraryShelf.neighbour(of: "favorites:b", in: rows, leaving: copies), "a")
        XCTAssertEqual(LibraryShelf.neighbour(of: "recent:b", in: rows, leaving: copies), "recent:c")
        XCTAssertEqual(LibraryShelf.neighbour(of: "x", in: [["x"], [], ["y", "z"]]), "y")
        XCTAssertEqual(LibraryShelf.neighbour(of: "x", in: [["w"], ["x"]]), "w")
        XCTAssertNil(LibraryShelf.neighbour(of: "x", in: [["x"]]))
    }

    /// A hide edits the cached catalog in place and keeps its age; a show adds the title back;
    /// the last title gone leaves no cache, and with no cache nothing is written.
    func testTheCachedCatalogFollowsAHide() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("hide-cache-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = LibraryCache(directory: directory)
        await cache.noteHidden("a", back: nil, hostID: "h", profile: "kid")
        let none = await cache.load(hostID: "h", profile: "kid")
        XCTAssertNil(none)

        await cache.store([game("a"), game("b")], hostID: "h", profile: "kid")
        let stored = await cache.load(hostID: "h", profile: "kid")
        await cache.noteHidden("a", back: nil, hostID: "h", profile: "kid")
        let hidden = await cache.load(hostID: "h", profile: "kid")
        XCTAssertEqual(hidden?.games.map(\.id), ["b"])
        XCTAssertNotNil(stored?.fetchedAt)
        XCTAssertEqual(hidden?.fetchedAt, stored?.fetchedAt)
        let box = await cache.load(hostID: "h", profile: nil)
        XCTAssertNil(box, "the box's catalog is another file")

        await cache.noteHidden("a", back: game("a"), hostID: "h", profile: "kid")
        let shown = await cache.load(hostID: "h", profile: "kid")
        XCTAssertEqual(shown?.games.map(\.id), ["b", "a"])

        await cache.noteHidden("a", back: nil, hostID: "h", profile: "kid")
        await cache.noteHidden("b", back: nil, hostID: "h", profile: "kid")
        let empty = await cache.load(hostID: "h", profile: "kid")
        XCTAssertNil(empty)
    }
}
