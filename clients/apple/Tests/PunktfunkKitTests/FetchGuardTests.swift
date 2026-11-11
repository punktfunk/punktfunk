// FetchGuard: which library work may touch the console's open shelf.

import XCTest

@testable import PunktfunkKit

final class FetchGuardTests: XCTestCase {
    private let onA = FetchGuard(fp: "aa", serial: 3)

    /// A list fetch fills only the shelf the console has open.
    func testAListFetchIsForTheOpenShelf() {
        XCTAssertTrue(onA.accepts(.list, fp: "aa"))
        XCTAssertFalse(onA.accepts(.list, fp: "bb"))
    }

    /// A newer list fetch owns the shelf by the time a slow host answers.
    func testAnAnswerFromAnOlderFetchIsDropped() {
        XCTAssertTrue(onA.accepts(.answer(serial: 3), fp: "aa"))
        XCTAssertFalse(onA.accepts(.answer(serial: 2), fp: "aa"))
    }

    /// A re-read of what the open shelf's host runs goes ahead.
    func testARefreshForTheOpenShelfRuns() {
        XCTAssertTrue(onA.accepts(.refresh, fp: "aa"))
    }
}
