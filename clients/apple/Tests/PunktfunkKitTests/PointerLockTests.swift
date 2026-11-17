// PointerLock: a row per iPad pointer-lock rule, against the pure state the controller steps.

import XCTest

@testable import PunktfunkKit

final class PointerLockTests: XCTestCase {
    /// Wanted, not held, frontmost and full screen: the state every re-ask expects.
    private let unlocked = PointerLock.Facts(wanted: true, granted: false, sceneCanHold: true)

    /// A drop while the lock is still wanted asks again, a turn later.
    func testADropWhileWantedReasksNextTurn() {
        var lock = PointerLock()
        XCTAssertEqual(lock.step(.lockChanged(ours: true, at: 10), unlocked), .reaskNextTurn)
        XCTAssertEqual(lock.step(.reask, unlocked), .beginEdge)
    }

    /// A deliberate release clears `captured` first, so its drop is never fought;
    /// another scene's change and a grant are not drops.
    func testOnlyAnUnwantedDropOfOurSceneReasks() {
        var lock = PointerLock()
        var released = unlocked
        released.wanted = false
        XCTAssertEqual(lock.step(.lockChanged(ours: true, at: 10), released), .none)
        XCTAssertEqual(lock.step(.lockChanged(ours: false, at: 10), unlocked), .none)
        var held = unlocked
        held.granted = true
        XCTAssertEqual(lock.step(.lockChanged(ours: true, at: 10), held), .none)
        var noScene = unlocked
        noScene.granted = nil
        XCTAssertEqual(lock.step(.lockChanged(ours: true, at: 10), noScene), .none)
    }

    /// The drop re-ask is a backstop; a grant revoked at once re-asks once a second.
    func testDropReasksAreOneASecond() {
        var lock = PointerLock()
        XCTAssertEqual(lock.step(.lockChanged(ours: true, at: 10), unlocked), .reaskNextTurn)
        XCTAssertEqual(lock.step(.lockChanged(ours: true, at: 10.5), unlocked), .none)
        XCTAssertEqual(
            lock.step(.lockChanged(ours: true, at: 10 + PointerLock.dropGap), unlocked),
            .reaskNextTurn)
    }

    /// A click up while captured but unlocked asks for the lock back.
    func testAClickWhileUnlockedReasks() {
        var lock = PointerLock()
        XCTAssertEqual(lock.step(.reask, unlocked), .beginEdge)
    }

    /// A lock declined on the first ask is asked again when the scene goes active;
    /// the ask is one false→true edge, and an ask during it folds in.
    func testAReaskIsOneEdge() {
        var lock = PointerLock()
        XCTAssertTrue(lock.prefersLocked(wanted: true))
        XCTAssertEqual(lock.step(.reask, unlocked), .beginEdge)
        XCTAssertFalse(lock.prefersLocked(wanted: true))
        XCTAssertEqual(lock.step(.reask, unlocked), .none)
        XCTAssertEqual(lock.step(.edgeEnded, unlocked), .endEdge)
        XCTAssertTrue(lock.prefersLocked(wanted: true))
        XCTAssertFalse(lock.prefersLocked(wanted: false))
    }

    /// A held or unwanted lock is never re-asked.
    func testAHeldOrUnwantedLockIsNotAsked() {
        var lock = PointerLock()
        var held = unlocked
        held.granted = true
        XCTAssertEqual(lock.step(.reask, held), .none)
        var released = unlocked
        released.wanted = false
        XCTAssertEqual(lock.step(.reask, released), .none)
        XCTAssertFalse(lock.edgeInFlight)
    }

    /// A windowed or background scene can't hold the lock, so it is never asked.
    func testAWindowedSceneIsNotAsked() {
        var lock = PointerLock()
        var windowed = unlocked
        windowed.sceneCanHold = false
        XCTAssertEqual(lock.step(.reask, windowed), .none)
        XCTAssertEqual(lock.step(.lockChanged(ours: true, at: 10), windowed), .reaskNextTurn)
        XCTAssertEqual(lock.step(.reask, windowed), .none)
    }

    /// The layout pass that makes a windowed scene fill again asks; one that keeps filling
    /// does not.
    func testOnlyARemaximiseAsksOnLayout() {
        var lock = PointerLock()
        XCTAssertEqual(lock.step(.layout(fills: true), unlocked), .none)
        var windowed = unlocked
        windowed.sceneCanHold = false
        XCTAssertEqual(lock.step(.layout(fills: false), windowed), .none)
        XCTAssertEqual(lock.step(.layout(fills: true), unlocked), .beginEdge)
        XCTAssertEqual(lock.step(.edgeEnded, unlocked), .endEdge)
        XCTAssertEqual(lock.step(.layout(fills: true), unlocked), .none)
    }

    /// Only a primary press engages capture from a click.
    func testOnlyAPrimaryPressEngagesCapture() {
        XCTAssertTrue(PointerLock.clickEngagesCapture(button: 1, down: true))
        XCTAssertFalse(PointerLock.clickEngagesCapture(button: 1, down: false))
        XCTAssertFalse(PointerLock.clickEngagesCapture(button: 2, down: true))
        XCTAssertFalse(PointerLock.clickEngagesCapture(button: 3, down: true))
    }
}
