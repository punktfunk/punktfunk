// DialRecovery.decide: a row per step of a failed dial's recovery, in the order it is tried.

import Foundation
import XCTest

@testable import PunktfunkKit
import PunktfunkShared

final class DialRecoveryTests: XCTestCase {
    private func decide(
        _ error: Error, wake: Duration? = nil, pinned: Bool = true, requestAccess: Bool = false,
        callerRecovers: Bool = false
    ) -> DialRecovery {
        DialRecovery.decide(
            error, wakeDelay: wake, hostName: "Desk", pinned: pinned,
            requestAccess: requestAccess, callerRecovers: callerRecovers)
    }

    private func reason(_ text: String) -> DialRecovery {
        .alert(L10n.dialHostReason(host: "Desk", reason: text))
    }

    /// A profile the host doesn't list goes to the caller first; its refusal shows otherwise.
    func testAnUnknownProfileAsksTheCallerFirst() {
        XCTAssertEqual(
            decide(PunktfunkClientError.rejected(.profileUnknown), callerRecovers: true),
            .profileUnknown(otherwise: reason(HostRejection.profileUnknown.userMessage)))
    }

    /// A locked profile hands the host's sentence to the caller, and shows it otherwise.
    func testALockedProfileAsksTheCallerWithTheHostsWords() {
        XCTAssertEqual(
            decide(PunktfunkClientError.rejected(.profileLocked, said: "Locked")),
            .profileLocked(said: "Locked", otherwise: reason("Locked")))
    }

    /// A host still waking is dialed again rather than shown as a refusal.
    func testAWakingHostIsDialedAgain() {
        XCTAssertEqual(
            decide(PunktfunkClientError.rejected(.setupFailed), wake: .seconds(2)),
            .wakeRedial(after: .seconds(2)))
        XCTAssertEqual(
            decide(PunktfunkClientError.rejected(.setupFailed)),
            reason(HostRejection.setupFailed.userMessage))
    }

    /// A host that answered states its own reason, even to a caller with a wake wait.
    func testARefusalShowsTheHostsReason() {
        XCTAssertEqual(
            decide(PunktfunkClientError.rejected(.busy), callerRecovers: true),
            reason(HostRejection.busy.userMessage))
    }

    /// An unreachable host goes to the caller's wake wait; without one, the alert says so.
    func testAnUnreachableHostGoesToTheCaller() {
        XCTAssertEqual(decide(PunktfunkClientError.connectFailed, callerRecovers: true), .handBack)
        XCTAssertEqual(
            decide(PunktfunkClientError.connectFailed),
            .alert(L10n.appleDialUnreachablePinned(host: "Desk")))
        XCTAssertEqual(
            decide(PunktfunkClientError.connectFailed, requestAccess: true, callerRecovers: true),
            .alert(String(localized: L10n.errorRequestFailed)))
    }

    /// Every recovery drops the launch hold before it runs.
    func testEveryRecoveryDropsTheLaunchHold() {
        let all: [DialRecovery] = [
            decide(PunktfunkClientError.rejected(.profileUnknown)),
            decide(PunktfunkClientError.rejected(.profileLocked)),
            decide(PunktfunkClientError.rejected(.setupFailed), wake: .seconds(2)),
            decide(PunktfunkClientError.connectFailed),
            decide(PunktfunkClientError.connectFailed, callerRecovers: true),
        ]
        for recovery in all { XCTAssertFalse(recovery.keepsLaunchHold, "\(recovery)") }
    }
}
