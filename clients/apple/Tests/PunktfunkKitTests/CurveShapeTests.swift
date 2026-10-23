// The Vision Pro screen's geometry at its stops: flat, a stop the box holds, and one it lifts.

import XCTest

import PunktfunkShared
@testable import PunktfunkKit

final class CurveShapeTests: XCTestCase {
    /// A 1.6 m chord in the 0.45 m-deep volume, tall enough that the height never narrows it.
    private func shape(_ curve: ScreenCurve) -> CurveShape {
        CurveShape(box: [1.62, 2, 0.45], aspect: 16 / 9, radius: curve.radius)
    }

    func testFlatHasNoSagAndLinearUV() {
        let flat = shape(.flat)
        XCTAssertNil(flat.radius)
        XCTAssertEqual(flat.halfChord, 0.8, accuracy: 1e-5)
        for x: Float in [-0.8, -0.3, 0, 0.5, 0.8] {
            XCTAssertEqual(flat.surfaceZ(atX: x), 0)
        }
        let quarter = flat.uv(at: [flat.halfChord / 2, flat.height / 2, 0])
        XCTAssertEqual(quarter.x, 0.75, accuracy: 1e-5)
        XCTAssertEqual(quarter.y, 0, accuracy: 1e-5)
        let corner = flat.uv(at: [-flat.halfChord, -flat.height / 2, 0])
        XCTAssertEqual(corner.x, 0, accuracy: 1e-5)
        XCTAssertEqual(corner.y, 1, accuracy: 1e-5)
    }

    func testAStopTheBoxHoldsKeepsItsRadius() {
        let curved = shape(.r1800)
        XCTAssertEqual(curved.radius, 1.8)
        XCTAssertEqual(curved.halfChord, 0.8, accuracy: 1e-5)
    }

    func testATighterStopIsLiftedToTheBoxsFit() {
        let lifted = shape(.r800)
        // 800R over 1.6 m sags 0.8 m; the box holds 0.41 m, the depth less the margins and lift.
        let sag: Float = 0.45 - 2 * 0.01 - CurveShape.ringLift
        let fitted = (0.8 * 0.8 + sag * sag) / (2 * sag)
        XCTAssertEqual(lifted.radius ?? 0, fitted, accuracy: 1e-4)
        XCTAssertEqual(-lifted.surfaceZ(atX: 0), sag, accuracy: 1e-4)
        XCTAssertEqual(lifted.halfChord, 0.8, accuracy: 1e-5)
    }

    func testOlderStoredNamesMapToStops() {
        XCTAssertEqual(ScreenCurve(name: "curved"), .r1800)
        XCTAssertEqual(ScreenCurve(name: "window"), .flat)
        XCTAssertEqual(ScreenCurve(name: "environment"), .flat)
        XCTAssertEqual(ScreenCurve(name: nil), .flat)
        XCTAssertEqual(ScreenCurve(name: "800r"), .r800)
        XCTAssertEqual(ScreenCurve.r800.radius, 0.8)
        XCTAssertEqual(ScreenCurve.r4000.label, "4000R")
        XCTAssertEqual(ScreenCurve.flat.label, "Flat")
    }
}
