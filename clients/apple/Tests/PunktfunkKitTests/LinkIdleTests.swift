// LinkIdle: whether a window is free to start what a link asks for.

import XCTest

@testable import PunktfunkKit

final class LinkIdleTests: XCTestCase {
    private func idle(
        _ phase: SessionPhase = .idle, redial: Bool = false, waking: Bool = false,
        fetch: Bool = false
    ) -> Bool {
        LinkIdle.isIdle(phase: phase, redialPending: redial, waking: waking, preDialFetch: fetch)
    }

    /// A window with no session and nothing on its way to one is free for a link.
    func testAnIdleWindowTakesALink() {
        XCTAssertTrue(idle())
    }

    /// A dial, a trust prompt or a stream is never preempted by a link.
    func testASessionUnderWayIsBusy() {
        let phases: [SessionPhase] = [.connecting, .awaitingTrust(fingerprint: Data([1])), .streaming]
        for phase in phases {
            XCTAssertFalse(idle(phase), "\(phase)")
        }
    }

    /// A lost session's pending redial, a wake wait and a tap's profile fetch each lead to a
    /// dial, so a link would start a second flow over them.
    func testAFlowOnItsWayToADialIsBusy() {
        XCTAssertFalse(idle(redial: true), "redial pending")
        XCTAssertFalse(idle(waking: true), "wake wait")
        XCTAssertFalse(idle(fetch: true), "pre-dial profile fetch")
    }

    /// The router answers a window waking a host as it answers a streaming one: a link to
    /// another host is refused, and one to the host being woken is already here.
    func testALinkDuringAWakeIsAnsweredAsDuringAStream() throws {
        let waking = StoredHost(name: "Desk", address: "10.0.0.2")
        let other = StoredHost(name: "Couch", address: "10.0.0.3")
        let session = DeepLinkRouter.SessionState(isIdle: idle(waking: true), activeHostID: waking.id)
        func resolve(_ host: StoredHost) throws -> DeepLinkRouter.Outcome {
            let url = try XCTUnwrap(URL(string: "punktfunk://connect/\(host.id.uuidString)"))
            return DeepLinkRouter.resolve(
                link: try DeepLink(url: url), hosts: [waking, other],
                catalog: PresetCatalog(presets: []), session: session, browse: false)
        }
        XCTAssertEqual(try resolve(other), .notice(String(localized: L10n.linkSessionRunning)))
        XCTAssertEqual(try resolve(waking), .alreadyHere)
    }
}
