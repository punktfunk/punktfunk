// ScreenOwner.resolve: a row per console-versus-app hand-off fix, then every combination.

import Foundation
import XCTest

@testable import PunktfunkKit

final class ScreenOwnerTests: XCTestCase {
    private static let phases: [SessionPhase] = [
        .idle, .connecting, .awaitingTrust(fingerprint: Data([1])), .streaming,
    ]

    private func owner(
        console: Bool = true, _ phase: SessionPhase, live: Bool? = nil, hold: Bool = false,
        error: Bool = false, fullscreen: Bool = false, appDriven: Bool = false,
        always: Bool = false
    ) -> ScreenOwner {
        let connected: Bool
        switch phase {
        case .idle, .connecting: connected = false
        case .awaitingTrust, .streaming: connected = true
        }
        return ScreenOwner.resolve(
            console: console, phase: phase, live: live ?? connected, consoleHold: hold,
            error: error, fullscreen: fullscreen, appDrivenFullscreen: appDriven,
            fullscreenAlways: always)
    }

    /// A session lost at the trust prompt never hands its redial the host's fingerprint.
    func testALossAtTheTrustPromptPinsNothing() {
        let live = Data([7]), dialed = Data([9])
        XCTAssertNil(SessionPhase.awaitingTrust(fingerprint: live).lossRedialPin(dialed: nil, live: live))
        XCTAssertEqual(SessionPhase.streaming.lossRedialPin(dialed: nil, live: live), live)
        XCTAssertEqual(SessionPhase.streaming.lossRedialPin(dialed: dialed, live: live), live)
        XCTAssertEqual(SessionPhase.connecting.lossRedialPin(dialed: dialed, live: live), dialed)
    }

    /// Without the console the app draws everything, prompts included.
    func testWithoutTheConsoleTheAppOwnsTheScreen() {
        for phase in Self.phases {
            let o = owner(console: false, phase, error: true)
            XCTAssertFalse(o.consoleOwnsScreen)
            XCTAssertFalse(o.showsConsole)
            XCTAssertTrue(o.showsError)
        }
        XCTAssertTrue(owner(console: false, .idle).showsHome)
        XCTAssertFalse(owner(console: false, .streaming).showsHome)
        XCTAssertTrue(owner(console: false, .streaming).streamTakesInput)
    }

    /// d2e782f9b: the console keeps the screen until a stream; the app's alerts stand down.
    func testTheConsoleKeepsTheScreenUntilAStream() {
        for phase in [SessionPhase.idle, .connecting] {
            let o = owner(phase, error: true)
            XCTAssertTrue(o.consoleOwnsScreen)
            XCTAssertTrue(o.showsHome)
            XCTAssertTrue(o.showsConsole)
            XCTAssertFalse(o.showsError) // 9072131cc: the console says it
        }
        let streaming = owner(.streaming, error: true)
        XCTAssertFalse(streaming.consoleOwnsScreen)
        XCTAssertFalse(streaming.showsHome)
        XCTAssertFalse(streaming.showsConsole)
        XCTAssertTrue(streaming.streamTakesInput)
        XCTAssertTrue(streaming.showsError)
    }

    /// 591f3c239: the launch hold keeps the console over the stream, which takes no input.
    func testTheLaunchHoldStaysOverTheStream() {
        let o = owner(.streaming, hold: true, error: true)
        XCTAssertTrue(o.consoleOwnsScreen)
        XCTAssertTrue(o.showsHome)
        XCTAssertTrue(o.showsConsole)
        XCTAssertFalse(o.streamTakesInput)
        XCTAssertFalse(o.showsError)
    }

    /// 591f3c239: a trust card is the stream view's, so the hold gives way to it.
    func testTheHoldGivesWayToATrustCard() {
        let o = owner(.awaitingTrust(fingerprint: Data([1])), hold: true, error: true)
        XCTAssertFalse(o.consoleOwnsScreen)
        XCTAssertFalse(o.showsHome)
        XCTAssertFalse(o.streamTakesInput)
        XCTAssertTrue(o.showsError)
    }

    /// 3e59eb6ef: the alert waits while a fullscreen this app drove leaves.
    func testTheErrorWaitsForAnAppFullscreenToLeave() {
        XCTAssertFalse(
            owner(console: false, .idle, error: true, fullscreen: true, appDriven: true).showsError)
        // The user's own fullscreen does not leave.
        XCTAssertTrue(owner(console: false, .idle, error: true, fullscreen: true).showsError)
        // Already out.
        XCTAssertTrue(owner(console: false, .idle, error: true, appDriven: true).showsError)
    }

    /// e30e3a090: under Always nothing leaves, so the error shows.
    func testAlwaysFullscreenShowsTheError() {
        let o = owner(
            console: false, .idle, error: true, fullscreen: true, appDriven: true, always: true)
        XCTAssertTrue(o.showsError)
    }

    /// Every combination keeps the rules each row above names.
    func testEveryCombinationHoldsTheRules() {
        // Seven flags, one bit each, against every phase.
        for bits in 0..<(1 << 7) {
            let on = { (n: Int) in bits & (1 << n) != 0 }
            for phase in Self.phases {
                let o = owner(
                    console: on(0), phase, live: on(1), hold: on(2), error: on(3),
                    fullscreen: on(4), appDriven: on(5), always: on(6))
                check(o, console: on(0), phase: phase, live: on(1), hold: on(2), error: on(3))
            }
        }
    }

    private func check(
        _ o: ScreenOwner, console: Bool, phase: SessionPhase, live: Bool, hold: Bool,
        error: Bool
    ) {
        let trust = if case .awaitingTrust = phase { true } else { false }
        let label = "\(console) \(phase) \(live) \(hold) \(error)"
        if !console {
            XCTAssertFalse(o.consoleOwnsScreen, label)
            XCTAssertFalse(o.showsConsole, label)
        }
        if o.showsError { XCTAssertTrue(error && !o.consoleOwnsScreen, label) }
        XCTAssertTrue(live || o.showsHome, "nothing mounted: \(label)")
        XCTAssertEqual(o.showsConsole, console && o.showsHome, label)
        if trust { XCTAssertFalse(o.streamTakesInput || o.consoleOwnsScreen, label) }
        if hold { XCTAssertFalse(o.streamTakesInput, label) }
    }
}
