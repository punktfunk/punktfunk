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
}
