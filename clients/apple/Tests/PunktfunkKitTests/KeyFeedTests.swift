import XCTest

@testable import PunktfunkKit

/// Pins `KeyFeed`: each physical key edge reaches the host once, whichever feed reports it,
/// and a feed that falls silent or speaks up again is noticed.
final class KeyFeedTests: XCTestCase {
    private let a = 0x04, b = 0x05

    /// Full Keyboard Access: GameController never speaks, so presses carry every key.
    func testPressesCarryTheKeysWhileGameControllerIsSilent() {
        var feed = KeyFeed()
        XCTAssertEqual(feed.takePress(usage: a, down: true), .take)
        XCTAssertFalse(feed.pressesCarriedKey)
        XCTAssertEqual(feed.takePress(usage: a, down: false), .take)
        XCTAssertTrue(feed.pressesCarriedKey, "a whole key carried by presses is the proof")
        XCTAssertEqual(feed.takePress(usage: b, down: true), .take)
        XCTAssertFalse(feed.gameControllerSpoke)
    }

    func testPressesStayQuietOnceGameControllerSpoke() {
        var feed = KeyFeed()
        XCTAssertTrue(feed.takeGameController(usage: a, down: true))
        XCTAssertEqual(feed.takePress(usage: a, down: true), .ignore)
        XCTAssertEqual(feed.takePress(usage: a, down: false), .ignore)
        XCTAssertTrue(feed.takeGameController(usage: a, down: false))
    }

    /// The press arrives first: GameController's press is the echo, its release ends the key.
    func testGameControllerEchoOfAPressFedKeyIsDropped() {
        var feed = KeyFeed()
        XCTAssertEqual(feed.takePress(usage: a, down: true), .take)
        XCTAssertFalse(feed.takeGameController(usage: a, down: true))
        XCTAssertEqual(feed.takePress(usage: a, down: false), .ignore)
        XCTAssertTrue(feed.takeGameController(usage: a, down: false))
        XCTAssertFalse(feed.pressesCarriedKey, "GameController finished the key: no proof")
        // The key is GameController's from here on.
        XCTAssertTrue(feed.takeGameController(usage: a, down: true))
    }

    /// Full Keyboard Access switched on mid-session: two keys pass GameController by, and
    /// presses carry from the second.
    func testTwoKeysThatPassGameControllerByHandTheKeysToPresses() {
        var feed = KeyFeed()
        _ = feed.takeGameController(usage: a, down: true) // held when it fell silent
        XCTAssertEqual(feed.takePress(usage: b, down: true), .ignore)
        XCTAssertEqual(feed.takePress(usage: b, down: false), .ignore)
        XCTAssertEqual(feed.takePress(usage: b, down: true), .ignore)
        XCTAssertEqual(feed.takePress(usage: b, down: false), .missed)
        XCTAssertTrue(feed.pressesCarriedKey)
        XCTAssertEqual(feed.takePress(usage: b, down: true), .take)
    }

    /// GameController mirrors each key a moment after its press ends, which happens on any
    /// keyboard: the count starts over, however many keys follow.
    func testALaggingGameControllerIsNotSilent() {
        var feed = KeyFeed()
        _ = feed.takeGameController(usage: a, down: true)
        _ = feed.takeGameController(usage: a, down: false)
        for _ in 0..<4 {
            XCTAssertEqual(feed.takePress(usage: b, down: true), .ignore)
            XCTAssertEqual(feed.takePress(usage: b, down: false), .ignore)
            XCTAssertTrue(feed.takeGameController(usage: b, down: true))
            XCTAssertTrue(feed.takeGameController(usage: b, down: false))
        }
        XCTAssertTrue(feed.gameControllerSpoke)
    }

    /// GameController speaks while the key is down: not a silent key, and not counted.
    func testAKeyGameControllerSpokeDuringIsNotCounted() {
        var feed = KeyFeed()
        _ = feed.takeGameController(usage: a, down: true)
        XCTAssertEqual(feed.takePress(usage: b, down: true), .ignore)
        XCTAssertEqual(feed.takePress(usage: b, down: false), .ignore) // one silent key
        XCTAssertEqual(feed.takePress(usage: b, down: true), .ignore)
        _ = feed.takeGameController(usage: a, down: false)
        XCTAssertEqual(feed.takePress(usage: b, down: false), .ignore)
        XCTAssertTrue(feed.gameControllerSpoke)
    }

    /// GameController has no element for the key, so its silence about it proves nothing.
    func testKeysGameControllerDoesNotModelAreNotCounted() {
        var feed = KeyFeed()
        _ = feed.takeGameController(usage: a, down: true)
        for _ in 0..<3 {
            XCTAssertEqual(feed.takePress(usage: b, down: true, modelled: false), .ignore)
            XCTAssertEqual(feed.takePress(usage: b, down: false, modelled: false), .ignore)
        }
        XCTAssertTrue(feed.gameControllerSpoke)
    }

    func testGameControllerSpeakingAgainDisprovesTheRepost() {
        var feed = KeyFeed()
        _ = feed.takePress(usage: a, down: true)
        _ = feed.takePress(usage: a, down: false)
        XCTAssertTrue(feed.pressesCarriedKey)
        XCTAssertTrue(feed.takeGameController(usage: b, down: true))
        XCTAssertFalse(feed.pressesCarriedKey)
        XCTAssertEqual(feed.takePress(usage: a, down: true), .ignore)
    }

    func testResetLetsPressesCarryAgain() {
        var feed = KeyFeed()
        _ = feed.takeGameController(usage: a, down: true)
        feed.reset()
        XCTAssertEqual(feed.takePress(usage: b, down: true), .take)
    }
}
