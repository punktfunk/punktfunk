// Deadline.first: the cap holds over work that ignores its cancel.

import Foundation
import XCTest

@testable import PunktfunkKit

final class DeadlineTests: XCTestCase {
    /// Work that cannot be cancelled, as a blocking transport call is.
    private static func deaf(_ seconds: TimeInterval, _ value: Int) -> @Sendable () async -> Int {
        {
            await withCheckedContinuation { done in
                DispatchQueue.global().asyncAfter(deadline: .now() + seconds) { done.resume() }
            }
            return value
        }
    }

    func testTheDeadlineDoesNotWaitForWorkThatIgnoresCancel() async {
        let started = Date()
        let answer = await Deadline.first(within: 0.1, fallback: -1, Self.deaf(2, 7))
        XCTAssertEqual(answer, -1)
        XCTAssertLessThan(Date().timeIntervalSince(started), 1)
    }

    func testAnAnswerInTimeWins() async {
        let answer = await Deadline.first(within: 2, fallback: -1, Self.deaf(0.05, 7))
        XCTAssertEqual(answer, 7)
    }
}
