// The motion frame conversion, pinned against the readings it was derived from.
//
// One physical DualSense was read twice — over raw HID (the pad's own report) and through
// GameController — so both frames come from the same controller in the same orientations
// rather than from two documents:
//
//   DualSense report frame: (Right, Up, Backward)   axis 0 pitch, 1 yaw, 2 roll
//   GameController frame:   (Right, Forward, Up)
//
// The numbers below are those measurements. They are the reason the conversion is `(x, z, -y)` and
// not one of the five other permutations that also move gravity to slot 1.

import XCTest

@testable import PunktfunkKit

final class GamepadMotionFrameTests: XCTestCase {
    private func wire(_ v: (Float, Float, Float)) -> (Float, Float, Float) {
        GamepadWire.appleMotionToWire(v)
    }

    /// Gravity at rest, face up. MEASURED: GameController read (+0.005, -0.192, +0.992) g while raw
    /// HID on the same pad read (+0.021, +0.997, +0.160). The conversion has to carry one into the
    /// other — including the small tilt term, which is what distinguishes this mapping from the one
    /// that merely gets gravity onto the right slot.
    func testRestingGravityLandsInTheDualSenseFrame() {
        let apple: (Float, Float, Float) = (0.005, -0.192, 0.992)
        let w = wire(apple)
        XCTAssertEqual(w.0, 0.005, accuracy: 0.001, "right stays on slot 0")
        XCTAssertEqual(w.1, 0.992, accuracy: 0.001, "up moves to slot 1 — the pad reads +1 g here")
        XCTAssertEqual(w.2, 0.192, accuracy: 0.001, "slot 2 is Backward, so GC's Forward negates")
        // The hardware's own reading of the same pose, to the precision two sessions of holding a
        // controller by hand can agree to.
        XCTAssertEqual(w.1, 0.997, accuracy: 0.02)
        XCTAssertEqual(w.2, 0.160, accuracy: 0.05)
    }

    /// The tilt term's SIGN is the whole point: sending Apple's y straight through would make a
    /// pad tilted nose-up report itself tilted nose-down.
    func testTheForeAftAxisIsNegatedNotJustMoved() {
        XCTAssertEqual(wire((0, 1, 0)).2, -1, "GC +y (Forward) is the wire's -Backward")
        XCTAssertEqual(wire((0, -1, 0)).2, 1)
        XCTAssertEqual(wire((0, 1, 0)).0, 0, "and it must not leak into the other slots")
        XCTAssertEqual(wire((0, 1, 0)).1, 0)
    }

    /// Each rotation, as measured, must reach the slot the wire reads it from: the wire's gyro is
    /// documented pitch/yaw/roll in slots 0/1/2, and the raw-HID run confirmed the pad agrees.
    func testEachRotationReachesItsWireSlot() {
        // Yaw is the reliable direct measurement — a continuous one-way spin, clockwise from above,
        // read as NEGATIVE on GC's z. It must arrive negative on slot 1, where the pad puts yaw.
        let yaw = wire((-0.2, 21.7, -122.2))
        XCTAssertEqual(yaw.1, -122.2, accuracy: 0.01)
        XCTAssertLessThan(yaw.1, 0, "clockwise-from-above is negative about +Up, both frames agree")

        // Pitch: nose-down about Right stays on slot 0 and keeps its sign.
        let pitch = wire((-79.4, 0, 0))
        XCTAssertEqual(pitch.0, -79.4, accuracy: 0.01)

        // Roll: about the fore-aft axis, which moves to slot 2 AND flips.
        let roll = wire((0, 61.8, 0))
        XCTAssertEqual(roll.2, -61.8, accuracy: 0.01)
    }

    /// A change of basis is linear and orthonormal: it may not stretch a vector, and applying it to
    /// gyro and to acceleration must be the same operation. Both are asserted because the capture
    /// path calls it twice, on two different quantities.
    func testConversionIsAnIsometry() {
        for v in [(1, 2, 3), (-4, 5, -6), (0, 0, 1), (7, 0, 0)] as [(Float, Float, Float)] {
            let w = wire(v)
            let before = (v.0 * v.0 + v.1 * v.1 + v.2 * v.2).squareRoot()
            let after = (w.0 * w.0 + w.1 * w.1 + w.2 * w.2).squareRoot()
            XCTAssertEqual(before, after, accuracy: 1e-4, "must not change magnitude")
        }
    }

    /// Right-handed in, right-handed out. A permutation with the wrong number of sign flips is a
    /// REFLECTION, which reads as plausible on every single axis and inverts every rotation — the
    /// exact failure this measurement exists to prevent.
    func testHandednessIsPreserved() {
        let x = wire((1, 0, 0))
        let y = wire((0, 1, 0))
        // x cross y must equal the image of z, not its negative.
        let cx = (x.1 * y.2 - x.2 * y.1, x.2 * y.0 - x.0 * y.2, x.0 * y.1 - x.1 * y.0)
        let z = wire((0, 0, 1))
        XCTAssertEqual(cx.0, z.0, accuracy: 1e-5)
        XCTAssertEqual(cx.1, z.1, accuracy: 1e-5)
        XCTAssertEqual(cx.2, z.2, accuracy: 1e-5)
    }
}
