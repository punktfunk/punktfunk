// ConnectPlan: the decisions a tap on a host takes before its dial, a row per rule.

import Foundation
import XCTest

@testable import PunktfunkKit

final class ConnectPlanTests: XCTestCase {
    private let advertised = Data(repeating: 7, count: 32)

    private func door(
        pinned: Bool = false, allowTofu: Bool? = nil, advertsTofu: Bool = false,
        fromLink: Bool = false, isDemo: Bool = false
    ) -> ConnectPlan.Door {
        ConnectPlan.door(
            pinned: pinned, allowTofu: allowTofu, advertsTofu: advertsTofu,
            advertisedFingerprint: advertised, fromLink: fromLink, isDemo: isDemo)
    }

    /// A pinned host dials on its pin whatever the caller or the advert says, and is asked who
    /// is playing.
    func testAPinnedHostDialsOnItsPin() {
        for allowTofu in [nil, false, true] {
            for advertsTofu in [false, true] {
                XCTAssertEqual(
                    door(pinned: true, allowTofu: allowTofu, advertsTofu: advertsTofu),
                    .dial(allowTofu: false, asksProfile: true))
            }
        }
    }

    /// The demo host has no management API to ask.
    func testTheDemoHostIsNotAskedForProfiles() {
        XCTAssertEqual(
            door(pinned: true, isDemo: true), .dial(allowTofu: false, asksProfile: false))
    }

    /// Rule 3a: an unpinned host trusts on first use only on the caller's word or a
    /// `pair=optional` advert, and is never asked for profiles.
    func testAnUnpinnedHostTrustsOnFirstUseOnlyWhenAllowed() {
        XCTAssertEqual(door(allowTofu: true), .dial(allowTofu: true, asksProfile: false))
        XCTAssertEqual(door(advertsTofu: true), .dial(allowTofu: true, asksProfile: false))
    }

    /// Rule 3b: any other unpinned host gets the approval choice, with the advert's certificate;
    /// the caller's no overrides a `pair=optional` advert.
    func testAnyOtherUnpinnedHostGetsTheApprovalChoice() {
        let choice = ConnectPlan.Door.approvalChoice(advertisedFingerprint: advertised, resumes: false)
        XCTAssertEqual(door(), choice)
        XCTAssertEqual(door(allowTofu: false, advertsTofu: true), choice)
        XCTAssertEqual(
            ConnectPlan.door(
                pinned: false, allowTofu: nil, advertsTofu: false, advertisedFingerprint: nil,
                fromLink: false, isDemo: false),
            .approvalChoice(advertisedFingerprint: nil, resumes: false))
    }

    /// Only a link's connect runs again once pairing pins the host.
    func testOnlyALinkResumesAfterPairing() {
        XCTAssertEqual(
            door(fromLink: true), .approvalChoice(advertisedFingerprint: advertised, resumes: true))
        XCTAssertEqual(
            door(allowTofu: true, fromLink: true), .dial(allowTofu: true, asksProfile: false))
    }
}
