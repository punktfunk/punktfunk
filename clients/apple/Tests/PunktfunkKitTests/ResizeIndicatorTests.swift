import XCTest

@testable import PunktfunkKit

final class ResizeIndicatorTests: XCTestCase {
    func testInactiveUntilSteered() {
        var r = ResizeIndicator()
        XCTAssertFalse(r.active)
        // A decoded frame with nothing pending is a no-op (session start / steady state).
        r.decoded(width: 1920, height: 1080)
        XCTAssertFalse(r.active)
    }

    func testSteeringActivatesAndDecodedTargetClears() {
        var r = ResizeIndicator()
        r.steering(width: 2560, height: 1440, now: 0)
        XCTAssertTrue(r.active)
        // A frame at a DIFFERENT size (the old mode still draining) doesn't clear it.
        r.decoded(width: 1920, height: 1080)
        XCTAssertTrue(r.active)
        // The target frame lands → clear.
        r.decoded(width: 2560, height: 1440)
        XCTAssertFalse(r.active)
    }

    func testTimeoutClearsWhenTargetNeverArrives() {
        var r = ResizeIndicator()
        r.steering(width: 2560, height: 1440, now: 10)
        r.tick(now: 12) // 2 s < timeout — still up
        XCTAssertTrue(r.active)
        r.tick(now: 12.6) // 2.6 s ≥ timeout — a rejected/capped switch clears
        XCTAssertFalse(r.active)
    }

    func testDragReArmsTimeoutOnEachNewTarget() {
        var r = ResizeIndicator()
        r.steering(width: 2000, height: 1200, now: 0)
        r.steering(width: 2200, height: 1200, now: 2) // target changed → since re-armed to 2
        r.tick(now: 4) // only 2 s since the last change — still up (drag isn't a timeout)
        XCTAssertTrue(r.active)
        r.tick(now: 4.6) // 2.6 s since the last change → clears
        XCTAssertFalse(r.active)
    }

    /// A host that reads the view sends a frame at it, any other host one at the mode: either
    /// clears, an old-size frame does not.
    func testFrameAtTheViewOrTheModeClears() {
        for frame: (UInt32, UInt32) in [(1_600, 900), (3_200, 1_800)] {
            var r = ResizeIndicator()
            r.steering(width: 3_200, height: 1_800, view: (1_600, 900), now: 0)
            r.decoded(width: 1_920, height: 1_080)
            XCTAssertTrue(r.active)
            r.decoded(width: frame.0, height: frame.1)
            XCTAssertFalse(r.active, "\(frame)")
        }
    }

    func testSteadyDragDoesNotResetTimeout() {
        var r = ResizeIndicator()
        r.steering(width: 2560, height: 1440, now: 0)
        r.steering(width: 2560, height: 1440, now: 1) // SAME target → since stays 0
        r.tick(now: 2.6) // 2.6 s since the ORIGINAL steer → clears (not reset by the repeat)
        XCTAssertFalse(r.active)
    }
}
