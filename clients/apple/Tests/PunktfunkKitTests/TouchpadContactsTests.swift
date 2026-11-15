import CoreGraphics
import PunktfunkCore
import XCTest

@testable import PunktfunkKit

/// The pure half of the touchpad sources (design/touchpad-passthrough.md §4). The capture halves
/// need UITouch / NSTouch instances and are checked on the device.
final class TouchpadContactsTests: XCTestCase {
    func testSizesAreTenthsOfAMillimetre() {
        XCTAssertEqual(TouchpadContacts.dmm(72, pointsPerInch: 72), 254)
        XCTAssertEqual(TouchpadContacts.screenPointsPerInch(pad: false), 153)
        XCTAssertEqual(TouchpadContacts.screenPointsPerInch(pad: true), 132)
        // A 390 × 844 pt phone is 64.7 × 140.1 mm.
        XCTAssertEqual(TouchpadContacts.dmm(390, pointsPerInch: 153), 647)
        XCTAssertEqual(TouchpadContacts.dmm(844, pointsPerInch: 153), 1401)
        // The wire field saturates; nothing, or nonsense, is unknown.
        XCTAssertEqual(TouchpadContacts.dmm(1_000_000, pointsPerInch: 72), UInt16.max)
        XCTAssertEqual(TouchpadContacts.dmm(0, pointsPerInch: 153), 0)
        XCTAssertEqual(TouchpadContacts.dmm(.nan, pointsPerInch: 153), 0)
    }

    func testTimestampsAreMicroseconds() {
        XCTAssertEqual(TouchpadContacts.micros(1.5), 1_500_000)
        XCTAssertEqual(TouchpadContacts.micros(0), 0)
    }

    func testAViewPointIsAFractionFromTheTopLeft() {
        let t = TouchpadContacts.touch(
            id: 7, at: CGPoint(x: 97.5, y: 633), in: CGSize(width: 390, height: 844))
        XCTAssertEqual(t.id, 7)
        XCTAssertEqual(t.x, 0.25, accuracy: 1e-6)
        XCTAssertEqual(t.y, 0.75, accuracy: 1e-6)
    }

    func testATrackpadPositionFlipsToTheTopLeft() {
        let t = TouchpadContacts.touch(id: 3, normalized: CGPoint(x: 0.2, y: 0.9))
        XCTAssertEqual(t.x, 0.2, accuracy: 1e-6)
        XCTAssertEqual(t.y, 0.1, accuracy: 1e-6) // near the top edge, away from the user
    }

    func testFrameModeKeepsMotionAndScrollHome() {
        let scroll = PunktfunkInputEvent.normalizedScroll(
            256, axis: 0, source: PUNKTFUNK_SCROLL_SOURCE_TOUCH, phase: PUNKTFUNK_SCROLL_PHASE_BEGIN)
        for taps in [false, true] {
            XCTAssertFalse(TouchpadContacts.passes(.mouseMove(dx: 3, dy: -2), hostTapsClick: taps))
            XCTAssertFalse(TouchpadContacts.passes(scroll, hostTapsClick: taps))
            XCTAssertTrue(TouchpadContacts.passes(.key(0x41, down: true), hostTapsClick: taps))
        }
    }

    func testFrameModeClicksComeFromWhicheverSideMakesTaps() {
        for down in [true, false] {
            for button: UInt32 in [1, 3] {
                let click = PunktfunkInputEvent.mouseButton(button, down: down)
                XCTAssertTrue(TouchpadContacts.passes(click, hostTapsClick: false))
                XCTAssertFalse(TouchpadContacts.passes(click, hostTapsClick: true))
            }
        }
    }

    /// The twist arms, and keeps the pair from the host, only before the pair travels away.
    func testTheTwistKeepsTheFingersOnlyBeforeTheyTravel() {
        let start = CGVector(dx: 100, dy: 0)
        let angle = CGFloat.pi / 15 // 12° clockwise on a y-down screen
        let turned = CGVector(dx: 100 * cos(angle), dy: 100 * sin(angle))
        let degrees = TouchTuning.twistDegrees(from: start, to: turned)
        XCTAssertEqual(degrees, 12, accuracy: 1e-6)
        XCTAssertEqual(TouchTuning.twistDegrees(from: turned, to: start), -12, accuracy: 1e-6)
        XCTAssertTrue(TouchTuning.twistArms(degrees: degrees, travel: 4))
        XCTAssertTrue(TouchTuning.twistArms(degrees: -degrees, travel: 4))
        XCTAssertFalse(TouchTuning.twistArms(degrees: 8, travel: 0)) // a scroll's wobble
        XCTAssertFalse(TouchTuning.twistArms(degrees: degrees, travel: TouchTuning.dialSlop))
    }
}

#if os(macOS)
import AppKit

final class TrackpadContactGateTests: XCTestCase {
    /// A size in millimetres as `NSTouch.deviceSize` reports it, points at 72 per inch.
    private func device(_ wMm: CGFloat, _ hMm: CGFloat) -> CGSize {
        CGSize(width: wMm / 25.4 * 72, height: hMm / 25.4 * 72)
    }

    func testOnlyATrackpadShapedDeviceGivesContacts() {
        XCTAssertTrue(TouchpadContacts.isTrackpad(device(160, 115))) // Magic Trackpad
        XCTAssertTrue(TouchpadContacts.isTrackpad(device(121, 78))) // a small laptop pad
        XCTAssertTrue(TouchpadContacts.isTrackpad(device(90, 60))) // the floor itself
        XCTAssertFalse(TouchpadContacts.isTrackpad(device(55, 95))) // Magic Mouse: portrait
        XCTAssertFalse(TouchpadContacts.isTrackpad(device(85, 55))) // landscape, under 90 mm
        XCTAssertFalse(TouchpadContacts.isTrackpad(device(100, 100))) // square is not landscape
        XCTAssertFalse(TouchpadContacts.isTrackpad(.zero))
        XCTAssertFalse(TouchpadContacts.isTrackpad(CGSize(width: CGFloat.nan, height: 0)))
    }

    /// With no qualifying finger down, the gate holds nothing back, whatever core answered.
    func testNoQualifyingFingerHoldsNothingBack() {
        let phases: [NSEvent.Phase] = [[], .mayBegin, .began, .changed, .stationary, .ended, .cancelled]
        for forwarded in [false, true] {
            var gate = TrackpadContactGate()
            gate.noteSurface(forwarded: forwarded, fingers: 0)
            XCTAssertFalse(gate.forwarding)
            for precise in [false, true] {
                for phase in phases {
                    XCTAssertFalse(gate.dropsScroll(precise: precise, phase: phase, momentumPhase: []))
                    XCTAssertFalse(gate.dropsScroll(precise: precise, phase: [], momentumPhase: phase))
                }
            }
            XCTAssertFalse(gate.forwarding)
        }
    }

    func testMotionStaysHomeOnlyWhileContactsAreForwarded() {
        var gate = TrackpadContactGate()
        XCTAssertFalse(gate.forwarding)
        gate.noteSurface(forwarded: true, fingers: 1)
        XCTAssertTrue(gate.forwarding)
        gate.noteSurface(forwarded: true, fingers: 0) // the last finger lifted
        XCTAssertFalse(gate.forwarding)
        gate.noteSurface(forwarded: false, fingers: 3) // handed back: today's paths
        XCTAssertFalse(gate.forwarding)
    }

    func testAForwardedScrollIsTheHostsThroughItsMomentum() {
        var gate = TrackpadContactGate()
        gate.noteSurface(forwarded: true, fingers: 2)
        XCTAssertTrue(gate.dropsScroll(precise: true, phase: .began, momentumPhase: []))
        XCTAssertTrue(gate.dropsScroll(precise: true, phase: .changed, momentumPhase: []))
        gate.noteSurface(forwarded: true, fingers: 0)
        XCTAssertTrue(gate.dropsScroll(precise: true, phase: .ended, momentumPhase: []))
        XCTAssertTrue(gate.dropsScroll(precise: true, phase: [], momentumPhase: .began))
        XCTAssertTrue(gate.dropsScroll(precise: true, phase: [], momentumPhase: .changed))
        XCTAssertTrue(gate.dropsScroll(precise: true, phase: [], momentumPhase: .ended))
        // The tail is over: a phaseless continuous surface and a wheel go out.
        XCTAssertFalse(gate.dropsScroll(precise: true, phase: [], momentumPhase: []))
        XCTAssertFalse(gate.dropsScroll(precise: false, phase: [], momentumPhase: []))
    }

    func testAScrollThatBeganBeforeTheContactsIsStillTheHosts() {
        var gate = TrackpadContactGate()
        XCTAssertFalse(gate.dropsScroll(precise: true, phase: .began, momentumPhase: []))
        gate.noteSurface(forwarded: true, fingers: 2)
        XCTAssertTrue(gate.dropsScroll(precise: true, phase: .changed, momentumPhase: []))
        gate.noteSurface(forwarded: true, fingers: 0)
        XCTAssertTrue(gate.dropsScroll(precise: true, phase: [], momentumPhase: .began))
    }

    func testAHandedBackScrollFlowsAsToday() {
        var gate = TrackpadContactGate()
        gate.noteSurface(forwarded: false, fingers: 3)
        XCTAssertFalse(gate.dropsScroll(precise: true, phase: .began, momentumPhase: []))
        XCTAssertFalse(gate.dropsScroll(precise: true, phase: .changed, momentumPhase: []))
        gate.noteSurface(forwarded: false, fingers: 0)
        XCTAssertFalse(gate.dropsScroll(precise: true, phase: .ended, momentumPhase: []))
        XCTAssertFalse(gate.dropsScroll(precise: true, phase: [], momentumPhase: .began))
    }

    func testAWheelIsNeverTheTrackpads() {
        var gate = TrackpadContactGate()
        gate.noteSurface(forwarded: true, fingers: 2)
        XCTAssertFalse(gate.dropsScroll(precise: false, phase: [], momentumPhase: []))
    }
}
#endif
