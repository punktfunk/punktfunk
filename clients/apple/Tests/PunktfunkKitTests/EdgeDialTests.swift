import CoreGraphics
import XCTest

@testable import PunktfunkKit

/// The passthrough dial opener's rule. It is the only gesture read off a passthrough stream, so
/// what it refuses matters more than what it accepts: every finger it does not claim is one the
/// game gets, and a false positive steals two of them mid-play.
final class EdgeDialTests: XCTestCase {
    private let width: CGFloat = 800

    private func pull(_ edge: DialEdge, from y: CGFloat, by dx: CGFloat) -> EdgeTrack {
        let x: CGFloat = edge == .left ? 4 : width - 4
        let now: CGFloat = edge == .left ? x + dx : x - dx
        return EdgeTrack(edge: edge, start: CGPoint(x: x, y: y), now: CGPoint(x: now, y: y))
    }

    func testOnlyABezelLandingStartsAPull() {
        XCTAssertEqual(EdgeDial.edge(of: CGPoint(x: 2, y: 100), width: width), .left)
        XCTAssertEqual(EdgeDial.edge(of: CGPoint(x: width - 2, y: 100), width: width), .right)
        // The middle of the screen is the game's, however far a finger then travels.
        XCTAssertNil(EdgeDial.edge(of: CGPoint(x: 400, y: 100), width: width))
        XCTAssertNil(EdgeDial.edge(of: CGPoint(x: EdgeDial.strip + 1, y: 100), width: width))
    }

    func testTwoFingersPulledInTogetherOpenTheDial() {
        let a = pull(.left, from: 300, by: EdgeDial.travel)
        let b = pull(.left, from: 360, by: EdgeDial.travel + 20)
        XCTAssertTrue(EdgeDial.completes(a, b))
        // It opens between them, so the ring arrives under the hand that asked.
        let c = EdgeDial.centre(a, b)
        XCTAssertEqual(c.y, 330, accuracy: 0.001)
    }

    func testAPullThatStopsShortIsJustTouches() {
        let a = pull(.left, from: 300, by: EdgeDial.travel)
        let short = pull(.left, from: 360, by: EdgeDial.travel - 1)
        XCTAssertFalse(EdgeDial.completes(a, short), "one finger has not come far enough")
    }

    func testOppositeBezelsAreTwoTouchesNotOneGesture() {
        let a = pull(.left, from: 300, by: EdgeDial.travel * 2)
        let b = pull(.right, from: 300, by: EdgeDial.travel * 2)
        XCTAssertFalse(EdgeDial.completes(a, b))
    }

    func testFingersTooFarApartAreNotOneHand() {
        let a = pull(.left, from: 0, by: EdgeDial.travel)
        let far = pull(.left, from: EdgeDial.spread + 10, by: EdgeDial.travel)
        XCTAssertFalse(EdgeDial.completes(a, far), "two players, not one pull")
    }
}
