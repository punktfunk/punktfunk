import GameController
import XCTest

@testable import PunktfunkKit

/// A slot that forwards nothing still runs its shortcuts, so it claims the system gesture of
/// every element they read (`GamepadCapture.elements`). A button left out stays the system's and
/// its shortcut never completes; on tvOS that is the exit hold, the only controller way out.
@MainActor
final class GamepadShortcutElementsTests: XCTestCase {
    func testEveryButtonAShortcutCanReadHasAnElement() {
        for bit in GamepadWire.allButtons {
            XCTAssertNotNil(GamepadCapture.elements[bit], "wire bit \(bit) has no element to claim")
        }
    }

    func testTheDefaultExitHoldClaimsItsFourButtons() {
        for client in [Shortcuts.Client.apple, .mac] {
            let engine = Shortcuts.Engine(Shortcuts.defaults(client: client).bindings, device: .pad)
            let claimed = Set(GamepadCapture.elements(for: engine.buttons))
            for alias in [GCInputLeftShoulder, GCInputRightShoulder, GCInputButtonMenu, GCInputButtonOptions] {
                XCTAssertTrue(claimed.contains(alias), "\(client): \(alias)")
            }
        }
    }

    /// A synthetic tap's presses go `tapPress` apart and lift together, last first.
    func testTapEventsSpaceThePressesAndLiftTogether() {
        let events = GamepadCapture.tapEvents([GamepadWire.guide, GamepadWire.a])
        XCTAssertEqual(events.map(\.bit), [GamepadWire.guide, GamepadWire.a, GamepadWire.a, GamepadWire.guide])
        XCTAssertEqual(events.map(\.down), [true, true, false, false])
        XCTAssertEqual(events.map(\.after), [0, 0.05, 0.1, 0.1])
    }

    /// Quick access held on a pad without that button is Guide, then A `tapPress` later. Let go
    /// inside that window, the releases wait behind A's press: A must not go down last.
    func testAHoldReleasedEarlyLiftsBehindItsOwedPress() {
        var owed = OwedPresses()
        let t0 = ContinuousClock.now
        let down = owed.push(
            [(GamepadWire.guide, true, 0), (GamepadWire.a, true, 0.05)], pad: 1, now: t0)
        XCTAssertEqual(down.map(\.bit), [GamepadWire.guide])
        let up = owed.push(
            [(GamepadWire.a, false, 0), (GamepadWire.guide, false, 0)], pad: 1,
            now: t0 + .milliseconds(10))
        XCTAssertTrue(up.isEmpty, "A's press is still owed")
        XCTAssertTrue(owed.take(dueBy: t0 + .milliseconds(40)).isEmpty)
        let sent = owed.take(dueBy: t0 + .milliseconds(50))
        XCTAssertEqual(sent.map(\.bit), [GamepadWire.a, GamepadWire.a, GamepadWire.guide])
        XCTAssertEqual(sent.map(\.down), [true, false, false])
        XCTAssertNil(owed.nextDue)
    }

    /// A flushed or closed slot is owed nothing: no press lands on a pad the host removed.
    func testAFlushDropsWhatItsPadIsOwed() {
        var owed = OwedPresses()
        let t0 = ContinuousClock.now
        _ = owed.push(GamepadCapture.tapEvents([GamepadWire.guide, GamepadWire.a]), pad: 0, now: t0)
        _ = owed.push(GamepadCapture.tapEvents([GamepadWire.guide]), pad: 1, now: t0)
        owed.drop(pad: 0)
        XCTAssertEqual(owed.take(dueBy: t0 + .seconds(1)).map(\.pad), [1])
    }
}
