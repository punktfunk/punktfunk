// The Steam Controller 2's half of `GamepadMenuInput.Snapshot` — the only half a test can
// reach, since XCTest cannot construct a GCController (see `GamepadUIEnvironment.isActive`).
// Pins the launcher's button contract (A confirms, B backs out, Y secondary, X tertiary) and
// the stick scaling onto the −1…1 the GameController path delivers, so an SC2 navigates the
// console UI the same way every other pad does.

import XCTest

@testable import PunktfunkKit

@MainActor
final class GamepadMenuSnapshotTests: XCTestCase {
    private func snapshot(buttons: UInt32) -> GamepadMenuInput.Snapshot {
        var state = Sc2Device.State()
        state.buttons = buttons
        return GamepadMenuInput.Snapshot(sc2: state)
    }

    func testFaceButtonsKeepTheLauncherContract() {
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnA).confirm)
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnB).back)
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnY).secondary)
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnX).tertiary)
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnLB).leftShoulder)
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnRB).rightShoulder)
        // Each bit drives ITS OWN field: an A press must not read as a confirm AND a back.
        let a = snapshot(buttons: Sc2Device.btnA)
        XCTAssertFalse(a.back || a.secondary || a.tertiary || a.leftShoulder || a.rightShoulder)
    }

    func testDpadBitsMapToTheirDirections() {
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnDpadUp).up)
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnDpadDown).down)
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnDpadLeft).left)
        XCTAssertTrue(snapshot(buttons: Sc2Device.btnDpadRight).right)
        let up = snapshot(buttons: Sc2Device.btnDpadUp)
        XCTAssertFalse(up.down || up.left || up.right)
    }

    func testNeutralStatePressesNothing() {
        XCTAssertEqual(GamepadMenuInput.Snapshot(sc2: Sc2Device.State()), snapshot(buttons: 0))
        let idle = snapshot(buttons: 0)
        XCTAssertFalse(idle.confirm || idle.back || idle.up || idle.down)
        XCTAssertEqual(idle.x, 0)
        XCTAssertEqual(idle.y, 0)
    }

    /// The device's raw i16 becomes the −1…1 the dead zone and hysteresis are written against,
    /// sign intact: +y is up on both sources, so a push up never scrolls the library down.
    func testSticksScaleToTheGameControllerRange() {
        var state = Sc2Device.State()
        state.lsX = 32767
        state.lsY = -32767
        var pad = GamepadMenuInput.Snapshot(sc2: state)
        XCTAssertEqual(pad.x, 1, accuracy: 0.001)
        XCTAssertEqual(pad.y, -1, accuracy: 0.001)

        // Half deflection stays half — the 0.5 dead zone is a real threshold, not a hair trigger.
        state.lsX = -16384
        state.lsY = 16384
        pad = GamepadMenuInput.Snapshot(sc2: state)
        XCTAssertEqual(pad.x, -0.5, accuracy: 0.001)
        XCTAssertEqual(pad.y, 0.5, accuracy: 0.001)
    }
}
