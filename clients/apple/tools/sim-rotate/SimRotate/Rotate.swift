import XCTest

/// Turns the simulator itself, as a hand would: `simctl` has no rotate, and an app that only
/// asks for a landscape scene leaves the system's status bar and safe area in portrait.
final class Rotate: XCTestCase {
    func testLandscape() {
        XCUIDevice.shared.orientation = .landscapeLeft
        Thread.sleep(forTimeInterval: 2)
    }
}
