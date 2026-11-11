// A library load whose last status read the host didn't answer, through the shelf it reloads.

import XCTest

@testable import PunktfunkKit

final class LibraryStatusMissTests: XCTestCase {
    /// R5-1: a reload whose status read misses keeps the Resume badge and the live download,
    /// so the download's poll goes on.
    func testAReloadWhoseStatusReadMissesKeepsTheBadges() async throws {
        let up = try JSONDecoder().decode(
            RunningGame.self, from: Data(#"{"app_id":"a","title":"A","state":"running"}"#.utf8))
        var shelf = LibraryShelf.State().applying(
            ([up], [HostDownload(appID: "d", state: "downloading")], 3))
        shelf.loading = true
        let load = LibraryLoad(
            hostID: "host", cache: nil, wake: nil, fetch: { [] }, status: { nil },
            retryEvery: 0)
        await load.events { shelf = LibraryShelf.reduce(shelf, $0, autoWake: false, canWake: false) }
        XCTAssertFalse(shelf.loading)
        XCTAssertEqual(Array(shelf.running.keys), ["a"])
        XCTAssertTrue(shelf.polls)
        XCTAssertEqual(shelf.grants, 3)
    }
}
