// The Match-window trigger discipline (design/midstream-resolution-resize.md D2), as pure
// functions — the same rules the session binary's `resize_decision` unit-tests: physical pixels
// even-floored and clamped ≥ 320×200, skip a size equal to the live mode, and request each
// distinct size at most once (so a rejected size / a host rollback can't loop).

import XCTest

@testable import PunktfunkKit

final class MatchWindowTests: XCTestCase {
    func testNormalizeEvenFloorsAndClamps() {
        // Odd pixels floor to even (the host rejects odd dimensions).
        let a = MatchWindow.normalize(widthPx: 1001, heightPx: 601)
        XCTAssertEqual(a.width, 1000)
        XCTAssertEqual(a.height, 600)
        // Already-even sizes pass through.
        let b = MatchWindow.normalize(widthPx: 2560, heightPx: 1440)
        XCTAssertEqual(b.width, 2560)
        XCTAssertEqual(b.height, 1440)
        // Tiny / zero clamp to the host floor.
        let c = MatchWindow.normalize(widthPx: 100, heightPx: 80)
        XCTAssertEqual(c.width, 320)
        XCTAssertEqual(c.height, 200)
        let z = MatchWindow.normalize(widthPx: 0, heightPx: -4)
        XCTAssertEqual(z.width, 320)
        XCTAssertEqual(z.height, 200)
    }

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
        // Already requested once → don't re-ask (covers a rejected size AND a host rollback:
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
