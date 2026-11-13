// The manual bitrate slider's log scale: its ends, its snapping, and a rate surviving the trip.

import XCTest

@testable import PunktfunkShared

final class BitrateScaleTests: XCTestCase {
    func testTheEndsAreTheScalesBounds() {
        XCTAssertEqual(BitrateScale.kbps(position: 0), 2_000)
        XCTAssertEqual(BitrateScale.kbps(position: 1), 3_000_000)
        XCTAssertEqual(BitrateScale.position(kbps: 2_000), 0, accuracy: 1e-12)
        XCTAssertEqual(BitrateScale.position(kbps: 3_000_000), 1, accuracy: 1e-12)
    }

    func testARateOutsideTheScaleSitsAtItsEnd() {
        XCTAssertEqual(BitrateScale.position(kbps: 0), 0)
        XCTAssertEqual(BitrateScale.position(kbps: 500), 0)
        XCTAssertEqual(BitrateScale.position(kbps: 10_000_000), 1)
    }

    func testRoundRatesSurviveTheTrip() {
        for kbps in [2_000, 5_000, 20_000, 47_000, 100_000, 150_000, 1_000_000, 3_000_000] {
            XCTAssertEqual(
                BitrateScale.kbps(position: BitrateScale.position(kbps: kbps)), kbps, "\(kbps)")
        }
    }

    func testASlidRateHasTwoSignificantFigures() {
        // 2000 · 1500^0.5 is 77 459.7.
        XCTAssertEqual(BitrateScale.kbps(position: 0.5), 77_000)
    }

    func testTheRateNeverFallsAsTheThumbMovesRight() {
        var last = 0
        for step in 0...200 {
            let kbps = BitrateScale.kbps(position: Double(step) / 200)
            XCTAssertGreaterThanOrEqual(kbps, last)
            last = kbps
        }
    }
}
