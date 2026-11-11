// LinkIdle: whether a window is free to start what a link asks for.

import XCTest

@testable import PunktfunkKit

final class LinkIdleTests: XCTestCase {
    /// A window with no session is free for a link.
    func testAnIdleWindowTakesALink() {
        XCTAssertTrue(LinkIdle.isIdle(phase: .idle))
    }

    /// A dial, a trust prompt or a stream is never preempted by a link.
    func testASessionUnderWayIsBusy() {
        let phases: [SessionPhase] = [.connecting, .awaitingTrust(fingerprint: Data([1])), .streaming]
        for phase in phases {
            XCTAssertFalse(LinkIdle.isIdle(phase: phase), "\(phase)")
        }
    }
}
