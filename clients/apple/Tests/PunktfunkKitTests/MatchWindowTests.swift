// The Match-window skip discipline (design/midstream-resolution-resize.md D2), as a pure
// function — the same rules the session binary's `resize_decision` unit-tests: skip a size equal
// to the live mode, and skip an ask equal to the last one sent (so a rejected size / a host
// rollback can't loop). The size itself is `RenderScale.apply`, held by the shared vectors.

import XCTest

@testable import PunktfunkKit

final class MatchWindowTests: XCTestCase {
    func testRequestSkipsEqualAndAlreadyRequested() {
        typealias Ask = MatchWindow.Ask
        // A new size (different from the live mode, not yet requested) → request it.
        let r = MatchWindow.request(
            target: Ask((1000, 600)), current: Ask((1280, 720)), lastRequested: Ask((800, 500)))
        XCTAssertEqual(r?.width, 1000)
        XCTAssertEqual(r?.height, 600)
        // Equal to the live mode → nothing to do.
        XCTAssertNil(MatchWindow.request(
            target: Ask((1280, 720)), current: Ask((1280, 720)), lastRequested: nil))
        // Same as the last ask → don't re-ask (covers a rejected size AND a host rollback:
        // accepted → rebuild failed → corrective ack restored the old mode must not loop).
        XCTAssertNil(MatchWindow.request(
            target: Ask((1000, 600)), current: Ask((1280, 720)), lastRequested: Ask((1000, 600))))
        // A clamped mode with a new view is an ask of its own.
        let viewed = Ask((4096, 2304), view: (3840, 2160))
        XCTAssertEqual(
            MatchWindow.request(
                target: viewed, current: Ask((4096, 2304), view: (2048, 1152)), lastRequested: nil),
            viewed)
    }
}
