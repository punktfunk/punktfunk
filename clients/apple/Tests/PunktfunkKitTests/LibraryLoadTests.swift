// The library load's order and retry rule, driven with faked host calls.

import XCTest
@testable import PunktfunkKit

final class LibraryLoadTests: XCTestCase {
    private let title = GameEntry(
        id: "steam:570", store: "steam", title: "Dota 2", art: Artwork())

    private func run(
        cache: LibraryCache? = nil, wakes: Bool, answers: [Error?]
    ) async -> (events: [String], fetches: Int) {
        var answers = answers
        var fetches = 0
        var events: [String] = []
        let load = LibraryLoad(
            hostID: "host", cache: cache, wake: wakes ? {} : nil,
            fetch: { [title] in
                fetches += 1
                if let error = answers.removeFirst() { throw error }
                return [title]
            },
            status: { ([], [], 7) },
            retryEvery: 0)
        await load.events { event in
            switch event {
            case .cached(let shelf): events.append("cached \(shelf.games.count)")
            case .waking: events.append("waking")
            case .fetched(let games): events.append("fetched \(games.count)")
            case .failed(let error, let hadCache): events.append("failed \(error) \(hadCache)")
            case .status(_, _, let grants): events.append("status \(grants ?? 0)")
            }
        }
        return (events, fetches)
    }

    func testARefusedCertificateIsAskedOnce() async {
        let r = await run(wakes: true, answers: [LibraryError.unauthorized])
        XCTAssertEqual(r.events, ["waking", "failed unauthorized false", "status 7"])
        XCTAssertEqual(r.fetches, 1)
    }

    func testAWokenHostIsAskedUntilItAnswers() async {
        let down = LibraryError.unreachable("timed out")
        let r = await run(wakes: true, answers: [down, down, nil])
        XCTAssertEqual(r.events, ["waking", "fetched 1", "status 7"])
        XCTAssertEqual(r.fetches, 3)
    }

    func testWithoutAWakeAnUnreachableHostIsAskedOnce() async {
        let r = await run(wakes: false, answers: [LibraryError.unreachable("refused")])
        XCTAssertEqual(r.events, ["failed unreachable(\"refused\") false", "status 7"])
        XCTAssertEqual(r.fetches, 1)
    }

    func testACachedShelfComesFirstAndSurvivesTheWakeWindow() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = LibraryCache(directory: directory)
        await cache.store([title], hostID: "host")
        let down = LibraryError.unreachable("timed out")
        let r = await run(
            cache: cache, wakes: true, answers: Array(repeating: down, count: LibraryLoad.wakeAttempts))
        XCTAssertEqual(
            r.events,
            ["cached 1", "waking", "failed unreachable(\"timed out\") true", "status 7"])
        XCTAssertEqual(r.fetches, LibraryLoad.wakeAttempts)
    }
}
