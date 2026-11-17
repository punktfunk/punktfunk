// The gamepad wire contract: which buttons are forwarded, GC→DualSense touchpad/motion
// conversions, and the player-LED-bits → GCControllerPlayerIndex map. All pure functions.

import GameController
import PunktfunkCore
import XCTest

@testable import PunktfunkKit

final class GamepadWireTests: XCTestCase {
    func testAllButtonsListsEachForwardedButtonOnce() {
        // The values come from the C header; this pins which of them `releaseAll` walks.
        let combined: UInt32 = GamepadWire.allButtons.reduce(0) { $0 | $1 }
        XCTAssertEqual(combined, 0x003F_F7FF)
        XCTAssertEqual(GamepadWire.allButtons.count, 21)
        XCTAssertEqual(GamepadWire.allButtons.count, Set(GamepadWire.allButtons).count)
    }

    /// A Kishi's bumpers are its printed L4/R4 and its M2/M1 back pair goes to L5/R5. An Elite
    /// has no bumpers, so its near back pair (P3/P1) is L4/R4 and the far one (P4/P2) L5/R5.
    @MainActor
    func testGripsFillL4R4ThenL5R5InGameControllerOrder() throws {
        guard #available(macOS 14.4, iOS 17.4, tvOS 17.4, *) else { throw XCTSkip("needs 17.4") }
        let backL = { GCButtonElementName.backLeftButton(position: $0).rawValue }
        let backR = { GCButtonElementName.backRightButton(position: $0).rawValue }
        let bumperL = GCButtonElementName.leftBumper.rawValue, bumperR = GCButtonElementName.rightBumper.rawValue
        let kishi: Set = [bumperL, bumperR, backL(0), backR(0)]
        XCTAssertEqual(GamepadCapture.grips(has: kishi.contains), [
            GamepadWire.paddle2: bumperL, GamepadWire.paddle1: bumperR,
            GamepadWire.paddle4: backL(0), GamepadWire.paddle3: backR(0),
        ])
        let elite: Set = [backL(0), backR(0), backL(1), backR(1)]
        XCTAssertEqual(GamepadCapture.grips(has: elite.contains), [
            GamepadWire.paddle2: backL(0), GamepadWire.paddle1: backR(0),
            GamepadWire.paddle4: backL(1), GamepadWire.paddle3: backR(1),
        ])
        XCTAssertEqual(GamepadCapture.grips { _ in false }, [:])
    }

    /// Steam reads Quick Access on a pad without the button as Guide held, then A. The tap
    /// keeps that order on the wire and lets go of A first. A pad with the button taps it.
    func testAQuickAccessTapHoldsGuideBeforeItPressesA() {
        let chord = GamepadCapture.tapEvents(GamepadWire.quickAccessPresses(hasButton: false))
        XCTAssertEqual(chord.map(\.bit), [GamepadWire.guide, GamepadWire.a, GamepadWire.a, GamepadWire.guide])
        XCTAssertEqual(chord.map(\.down), [true, true, false, false])
        XCTAssertEqual(chord.map(\.after), [0, 0.05, 0.1, 0.1])
        let button = GamepadCapture.tapEvents(GamepadWire.quickAccessPresses(hasButton: true))
        XCTAssertEqual(button.map(\.bit), [GamepadWire.misc1, GamepadWire.misc1])
        XCTAssertEqual(button.map(\.down), [true, false])
        XCTAssertEqual(button.map(\.after), [0, 0.05])
    }

    func testPadIndexRidesFlagsOnEveryPerPadEvent() {
        // The wire pad index is the low byte of `flags` (punktfunk_core::input) on button + axis.
        let btn = PunktfunkInputEvent.gamepadButton(GamepadWire.a, down: true, pad: 3)
        XCTAssertEqual(btn.kind, UInt8(PUNKTFUNK_INPUT_KIND_GAMEPAD_BUTTON.rawValue))
        XCTAssertEqual(btn.code, GamepadWire.a)
        XCTAssertEqual(btn.x, 1)
        XCTAssertEqual(btn.flags, 3)
        let axis = PunktfunkInputEvent.gamepadAxis(GamepadWire.axisRT, value: 200, pad: 5)
        XCTAssertEqual(axis.kind, UInt8(PUNKTFUNK_INPUT_KIND_GAMEPAD_AXIS.rawValue))
        XCTAssertEqual(axis.code, GamepadWire.axisRT)
        XCTAssertEqual(axis.x, 200)
        XCTAssertEqual(axis.flags, 5)
        // Pad 0 ⇒ flags 0: a single controller's events leave `flags` clear.
        XCTAssertEqual(PunktfunkInputEvent.gamepadButton(GamepadWire.a, down: false, pad: 0).flags, 0)
        XCTAssertEqual(PunktfunkInputEvent.gamepadAxis(GamepadWire.axisLSX, value: 0, pad: 0).flags, 0)
    }

    func testArrivalAndRemoveWireLayout() {
        // GamepadArrival (kind 14): code = the GamepadType wire byte, flags = pad index.
        let arrival = PunktfunkInputEvent.gamepadArrival(
            pref: PunktfunkConnection.GamepadType.dualSense.rawValue, pad: 2)
        XCTAssertEqual(arrival.kind, UInt8(PUNKTFUNK_INPUT_KIND_GAMEPAD_ARRIVAL.rawValue))
        XCTAssertEqual(arrival.code, PunktfunkConnection.GamepadType.dualSense.rawValue) // 2
        XCTAssertEqual(arrival.flags, 2)
        // The GamepadType raw values ARE the GamepadPref wire bytes the host resolves.
        XCTAssertEqual(PunktfunkConnection.GamepadType.xbox360.rawValue, 1)
        XCTAssertEqual(PunktfunkConnection.GamepadType.dualSense.rawValue, 2)
        XCTAssertEqual(PunktfunkConnection.GamepadType.xboxOne.rawValue, 3)
        XCTAssertEqual(PunktfunkConnection.GamepadType.dualShock4.rawValue, 4)
        // GamepadRemove (kind 13): flags = pad index (the core stamps the per-pad seq).
        let remove = PunktfunkInputEvent.gamepadRemove(pad: 7)
        XCTAssertEqual(remove.kind, UInt8(PUNKTFUNK_INPUT_KIND_GAMEPAD_REMOVE.rawValue))
        XCTAssertEqual(remove.flags, 7)
        // 16 addressable pads (punktfunk_core::input::MAX_PADS).
        XCTAssertEqual(GamepadWire.maxPads, 16)
    }

    func testAnExplicitControllerTypeIsWhatEveryPadDeclares() {
        // Every pad's arrival declares the explicit "Controller type", not the DETECTED kind: the
        // host honors an arrival's kind over the session default, so "emulate my DualSense as a
        // DualShock 4" would otherwise produce a DualSense.
        // (pf-client-core's `declared_kind` test is the same table.)
        XCTAssertEqual(
            GamepadManager.declaredKind(setting: .dualShock4, detected: .dualSense), .dualShock4)
        // Every physical pad in a mixed session follows the one explicit choice.
        for detected: PunktfunkConnection.GamepadType in [
            .dualSense, .xbox360, .switchPro, .dualSenseEdge,
        ] {
            XCTAssertEqual(
                GamepadManager.declaredKind(setting: .xbox360, detected: detected), .xbox360)
        }
        // Automatic keeps per-pad detection — otherwise a mixed session collapses to one type.
        XCTAssertEqual(GamepadManager.declaredKind(setting: .auto, detected: .dualSense), .dualSense)
        XCTAssertEqual(GamepadManager.declaredKind(setting: .auto, detected: .switchPro), .switchPro)
    }

    func testTouchpadConversionCorners() {
        // GC ±1 with +y up → wire 0...65535 with origin top-left, +y down.
        let topLeft = GamepadWire.touchpad(x: -1, y: 1)
        XCTAssertEqual(topLeft.x, 0)
        XCTAssertEqual(topLeft.y, 0)
        let bottomRight = GamepadWire.touchpad(x: 1, y: -1)
        XCTAssertEqual(bottomRight.x, 65535)
        XCTAssertEqual(bottomRight.y, 65535)
        let center = GamepadWire.touchpad(x: 0, y: 0)
        XCTAssertEqual(Int(center.x), 32768, accuracy: 1)
        XCTAssertEqual(Int(center.y), 32768, accuracy: 1)
        // Out-of-range input clamps instead of wrapping.
        let wild = GamepadWire.touchpad(x: 5, y: -7)
        XCTAssertEqual(wild.x, 65535)
        XCTAssertEqual(wild.y, 65535)
    }

    func testMotionScalingAndClamping() {
        // 20 raw LSB per deg/s — one full revolution per second (360 deg/s = 2π rad/s).
        XCTAssertEqual(GamepadWire.motionRaw(2 * .pi, scale: GamepadWire.gyroLSBPerRadS), 7200)
        // 1 g → 10000 raw.
        XCTAssertEqual(GamepadWire.motionRaw(1, scale: GamepadWire.accelLSBPerG), 10000)
        XCTAssertEqual(GamepadWire.motionRaw(-1, scale: GamepadWire.accelLSBPerG), -10000)
        // Saturation, not overflow.
        XCTAssertEqual(GamepadWire.motionRaw(100, scale: GamepadWire.accelLSBPerG), Int16.max)
        XCTAssertEqual(GamepadWire.motionRaw(-100, scale: GamepadWire.accelLSBPerG), Int16.min)
        XCTAssertEqual(GamepadWire.motionRaw(0, scale: GamepadWire.gyroLSBPerRadS), 0)
    }

    func testPlayerIndexMap() {
        // hid-playstation's DualSense player-LED patterns.
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0), .indexUnset)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b00100), .index1)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b01010), .index2)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b10101), .index3)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b11011), .index4)
        // Unknown patterns: lit-count fallback, clamped to GC's four indices.
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b00001), .index1)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b00011), .index2)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b11111), .index4)
        // Bits above the 5 LEDs are ignored.
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b1110_0000), .indexUnset)
    }
}
