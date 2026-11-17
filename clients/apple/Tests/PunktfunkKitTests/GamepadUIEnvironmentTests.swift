// GamepadUIEnvironment.isActive is pure — table-tested exhaustively over its inputs.

import XCTest

@testable import PunktfunkKit

final class GamepadUIEnvironmentTests: XCTestCase {
    private let connected = GamepadUIEnvironment.modeWhenConnected
    private let always = GamepadUIEnvironment.modeAlways

    /// The default mode is a plain AND of the connected controller and the switch.
    func testWhenConnectedIsAPlainAnd() {
        XCTAssertTrue(
            GamepadUIEnvironment.isActive(
                gamepadConnected: true, enabledSetting: true, mode: connected))
        XCTAssertFalse(
            GamepadUIEnvironment.isActive(
                gamepadConnected: true, enabledSetting: false, mode: connected))
        XCTAssertFalse(
            GamepadUIEnvironment.isActive(
                gamepadConnected: false, enabledSetting: true, mode: connected))
        XCTAssertFalse(
            GamepadUIEnvironment.isActive(
                gamepadConnected: false, enabledSetting: false, mode: connected))
    }

    /// Always drops the controller from the decision entirely — but NOT the switch, which stays
    /// the one way back to the touch UI.
    func testAlwaysIgnoresTheControllerButNotTheSwitch() {
        XCTAssertTrue(
            GamepadUIEnvironment.isActive(
                gamepadConnected: false, enabledSetting: true, mode: always))
        XCTAssertTrue(
            GamepadUIEnvironment.isActive(
                gamepadConnected: true, enabledSetting: true, mode: always))
        XCTAssertFalse(
            GamepadUIEnvironment.isActive(
                gamepadConnected: false, enabledSetting: false, mode: always))
        XCTAssertFalse(
            GamepadUIEnvironment.isActive(
                gamepadConnected: true, enabledSetting: false, mode: always))
    }

    /// A value a newer client wrote must wait for a controller, never strand this build in a
    /// layout it has no way back out of.
    func testUnknownModeWaitsForAController() {
        XCTAssertFalse(
            GamepadUIEnvironment.isActive(
                gamepadConnected: false, enabledSetting: true, mode: "whenever-i-say-so"))
        XCTAssertTrue(
            GamepadUIEnvironment.isActive(
                gamepadConnected: true, enabledSetting: true, mode: "whenever-i-say-so"))
        XCTAssertFalse(
            GamepadUIEnvironment.isActive(
                gamepadConnected: false, enabledSetting: true, mode: ""))
    }
}
