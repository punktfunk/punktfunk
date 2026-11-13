// The library load's order and retry rule, driven with faked host calls.

import CryptoKit
import XCTest
@testable import PunktfunkKit

final class LibraryLoadTests: XCTestCase {
    private let title = GameEntry(
        id: "steam:570", store: "steam", title: "Dota 2", art: Artwork())

    /// A load for the shelf of `profile`; the host answers as `served`.
    private func run(
        cache: LibraryCache? = nil, profile: String? = nil, served: String? = nil, wakes: Bool,
        answers: [Error?]
    ) async -> (events: [String], fetches: Int) {
        var answers = answers
        var fetches = 0
        var events: [String] = []
        let load = LibraryLoad(
            hostID: "host", profile: profile, cache: cache, wake: wakes ? {} : nil,
            fetch: { [title] in
                fetches += 1
                if let error = answers.removeFirst() { throw error }
                return ([title], served)
            },
            status: { ([], [], 7) },
            retryEvery: 0)
        await load.events { event in
            switch event {
            case .cached(let shelf): events.append("cached \(shelf.games.count)")
            case .waking: events.append("waking")
            case .fetched(let games): events.append("fetched \(games.count)")
            case .failed(let error, let hadCache): events.append("failed \(error) \(hadCache)")
            case .status(let answer): events.append("status \(answer?.grants ?? 0)")
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
        await cache.store([title], hostID: "host", profile: nil)
        let down = LibraryError.unreachable("timed out")
        let r = await run(
            cache: cache, wakes: true, answers: Array(repeating: down, count: LibraryLoad.wakeAttempts))
        XCTAssertEqual(
            r.events,
            ["cached 1", "waking", "failed unreachable(\"timed out\") true", "status 7"])
        XCTAssertEqual(r.fetches, LibraryLoad.wakeAttempts)
    }

    /// A profile's shelf never paints the box's cached titles, and a gone profile's fallback is
    /// cached as the box's, not as the profile's.
    func testAProfileShelfPaintsOnlyItsOwnCatalog() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = LibraryCache(directory: directory)
        await cache.store([title], hostID: "host", profile: nil)
        let kid = await run(cache: cache, profile: "kid", served: "kid", wakes: false, answers: [nil])
        XCTAssertEqual(kid.events, ["fetched 1", "status 7"])
        let own = await cache.load(hostID: "host", profile: "kid")
        XCTAssertEqual(own?.games.map(\.id), [title.id])

        let gone = await run(cache: cache, profile: "ben", served: nil, wakes: false, answers: [nil])
        XCTAssertEqual(gone.events, ["fetched 1", "status 7"])
        let ben = await cache.load(hostID: "host", profile: "ben")
        XCTAssertNil(ben)
    }

    /// The box's catalog keeps the file name it had before profiles, so an old file loads as the
    /// box's; forgetting the host takes every profile's with it.
    func testTheBoxCatalogKeepsItsOldFileAndForgetTakesEveryProfile() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let cache = LibraryCache(directory: directory)
        let old = try JSONEncoder().encode(CachedLibrary(games: [title], fetchedAt: Date()))
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let digest = SHA256.hash(data: Data("host".utf8)).map { String(format: "%02x", $0) }
        try old.write(to: directory.appendingPathComponent(digest.joined() + ".json"))
        let box = await cache.load(hostID: "host", profile: nil)
        XCTAssertEqual(box?.games.map(\.id), [title.id])
        let kid = await cache.load(hostID: "host", profile: "kid")
        XCTAssertNil(kid)

        await cache.store([title], hostID: "host", profile: "kid")
        await cache.forget(hostID: "host")
        let after = try FileManager.default.contentsOfDirectory(atPath: directory.path)
        XCTAssertEqual(after, [])
    }
}
