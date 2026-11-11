// BackgroundPolicy: what a scene going to the background does to the session.

import XCTest

@testable import PunktfunkKit

final class BackgroundPolicyTests: XCTestCase {
    /// The keep-alive holds a streaming session for the player's minutes.
    func testKeepAliveHoldsAStream() {
        XCTAssertEqual(
            BackgroundPolicy.decide(phase: .streaming, keepAlive: true, minutes: 10),
            .hold(minutes: 10))
    }

    /// Without it the stream ends, or it would run on unwatched.
    func testWithoutKeepAliveAStreamEnds() {
        XCTAssertEqual(BackgroundPolicy.decide(phase: .streaming, keepAlive: false, minutes: 10), .end)
    }

    /// Before the stream nothing happens: a dial that lands in the background is decided then.
    func testNothingBeforeTheStream() {
        let phases: [SessionPhase] = [.idle, .connecting, .awaitingTrust(fingerprint: Data([1]))]
        for phase in phases {
            for keepAlive in [true, false] {
                XCTAssertEqual(
                    BackgroundPolicy.decide(phase: phase, keepAlive: keepAlive, minutes: 10), .none,
                    "\(phase) keepAlive=\(keepAlive)")
            }
        }
    }
}
