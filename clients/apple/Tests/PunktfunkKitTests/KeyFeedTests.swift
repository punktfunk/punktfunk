import XCTest

@testable import PunktfunkKit

/// Pins `KeyFeed`: each physical key edge reaches the host once, whichever feed reports it.
final class KeyFeedTests: XCTestCase {
    private let a = 0x04, b = 0x05

    /// Full Keyboard Access: GameController never speaks, so presses carry every key.
    func testPressesCarryTheKeysWhileGameControllerIsSilent() {
        var feed = KeyFeed()
        XCTAssertTrue(feed.takePress(usage: a, down: true))
        XCTAssertTrue(feed.takePress(usage: a, down: false))
        XCTAssertTrue(feed.takePress(usage: b, down: true))
        XCTAssertFalse(feed.gameControllerSpoke)
    }

    func testPressesStayQuietOnceGameControllerSpoke() {
        var feed = KeyFeed()
        XCTAssertTrue(feed.takeGameController(usage: a, down: true))
        XCTAssertFalse(feed.takePress(usage: a, down: true))
        XCTAssertFalse(feed.takePress(usage: a, down: false))
        XCTAssertTrue(feed.takeGameController(usage: a, down: false))
    }

    /// The press arrives first: GameController's press is the echo, its release ends the key.
    func testGameControllerEchoOfAPressFedKeyIsDropped() {
        var feed = KeyFeed()
        XCTAssertTrue(feed.takePress(usage: a, down: true))
        XCTAssertFalse(feed.takeGameController(usage: a, down: true))
        XCTAssertFalse(feed.takePress(usage: a, down: false))
        XCTAssertTrue(feed.takeGameController(usage: a, down: false))
        // The key is GameController's from here on.
        XCTAssertTrue(feed.takeGameController(usage: a, down: true))
    }

    func testResetLetsPressesCarryAgain() {
        var feed = KeyFeed()
        _ = feed.takeGameController(usage: a, down: true)
        feed.reset()
        XCTAssertTrue(feed.takePress(usage: b, down: true))
    }
}
