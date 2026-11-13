// SessionEndRoute.decide: a row per branch `SessionModel.sessionEnded` takes.

import Foundation
import XCTest

@testable import PunktfunkKit
import PunktfunkShared

final class SessionEndRouteTests: XCTestCase {
    private let live = Data([7])
    private let dialedPin = Data([9])

    private func route(
        _ reason: PunktfunkConnection.SessionEndReason, phase: SessionPhase = .streaming,
        rejection: HostRejection? = nil, said: String? = nil, wake: Duration? = nil,
        pinned: Bool = false, dial: Bool = true, shelf: Bool = false
    ) -> SessionEndRoute {
        let host = StoredHost(
            name: "Den", address: "10.0.0.2", pinnedSHA256: pinned ? dialedPin : nil)
        return SessionEndRoute.decide(
            phase: phase, reason: reason, rejection: rejection, said: said, wakeDelay: wake,
            dialed: dial ? host : nil, live: live, shelf: shelf)
    }

    /// A host that closed before it could stream is dialed again, whatever else it said.
    func testAWakingHostIsDialedAgainFirst() {
        XCTAssertEqual(
            route(.hostError, rejection: .setupFailed, wake: .seconds(2), shelf: true),
            .wakeRedial(after: .seconds(2)))
        XCTAssertEqual(route(.lost, wake: .seconds(2)), .wakeRedial(after: .seconds(2)))
        // No dial to repeat: the close shows.
        XCTAssertEqual(
            route(.hostError, rejection: .setupFailed, wake: .seconds(2), dial: false),
            .message(.rejected(.setupFailed, said: nil)))
    }

    /// A session the player accepted redials pinned to the fingerprint it streamed from.
    func testALostSessionRedialsPinnedToItsHost() {
        XCTAssertEqual(route(.lost), .lossRedial(pin: live))
        XCTAssertEqual(route(.lost, pinned: true), .lossRedial(pin: live))
        XCTAssertEqual(route(.lost, dial: false), .message(.lost))
    }

    /// 314a2575a: a loss at the trust prompt never redials an unpinned host as trusted.
    func testALossAtTheTrustPromptRedialsOnlyAPinnedDial() {
        let prompt = SessionPhase.awaitingTrust(fingerprint: live)
        XCTAssertEqual(route(.lost, phase: prompt), .message(.lost))
        XCTAssertEqual(route(.lost, phase: prompt, pinned: true), .lossRedial(pin: dialedPin))
    }

    /// A typed close outranks the coarse reason, a loss included.
    func testATypedCloseSaysItsOwnReason() {
        XCTAssertEqual(
            route(.hostError, rejection: .accessExpired),
            .message(.rejected(.accessExpired, said: nil)))
        XCTAssertEqual(
            route(.lost, rejection: .hostPower, said: "Asleep"),
            .message(.rejected(.hostPower, said: "Asleep")))
    }

    /// The host's own sentence outranks the typed wording.
    func testTheHostsSentenceOutranksTheTypedWording() {
        let said = SessionEndRoute.Message.rejected(.accessExpired, said: "Gone")
        XCTAssertEqual(said.text(host: "Den"), L10n.dialHostReason(host: "Den", reason: "Gone"))
        let typed = SessionEndRoute.Message.rejected(.accessExpired, said: nil)
        XCTAssertEqual(
            typed.text(host: "Den"),
            L10n.dialHostReason(host: "Den", reason: HostRejection.accessExpired.userMessage))
    }

    /// A game exit goes back to the shelf it came from, and says nothing without one.
    func testAGameExitReturnsToItsShelf() {
        XCTAssertEqual(route(.gameExited, shelf: true), .library)
        XCTAssertEqual(route(.gameExited), SessionEndRoute.none)
    }

    /// Every other reason has its own sentence.
    func testEachReasonHasItsSentence() {
        XCTAssertEqual(route(.hostEnded, shelf: true), .message(.endedByHost))
        XCTAssertEqual(route(.local), .message(.endedByHost))
        XCTAssertEqual(route(.hostError), .message(.hostError))
        XCTAssertEqual(route(.none), .message(.unknown))
    }
}
