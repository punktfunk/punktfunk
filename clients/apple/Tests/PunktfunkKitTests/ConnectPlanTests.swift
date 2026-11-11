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

    private let anna = ListedProfile(id: "anna", displayName: "Anna")
    private let ben = ListedProfile(id: "ben", displayName: "Ben")
    private let annaPick = ProfilePick(id: "anna", displayName: "Anna")

    private func after(
        _ rows: [ListedProfile]?, failed: Bool = false, saved: ProfilePick? = nil,
        link: String? = nil
    ) -> ConnectPlan.AfterProfiles {
        ConnectPlan.afterProfiles(rows: rows, failed: failed, saved: saved, link: link)
    }

    /// A host that didn't answer in time dials the link's profile, else the saved pick, and
    /// names it only when it is the saved one.
    func testAFailedListDialsTheLinkOrTheSavedPick() {
        XCTAssertEqual(after(nil, failed: true), .dial(id: nil, name: nil))
        XCTAssertEqual(after(nil, failed: true, saved: annaPick), .dial(id: "anna", name: "Anna"))
        XCTAssertEqual(
            after(nil, failed: true, saved: annaPick, link: "ben"), .dial(id: "ben", name: nil))
        XCTAssertEqual(
            after(nil, failed: true, saved: annaPick, link: "anna"), .dial(id: "anna", name: "Anna"))
        // Rows from a failed answer are not read.
        XCTAssertEqual(after([anna, ben], failed: true), .dial(id: nil, name: nil))
    }

    /// A box without profiles dials with the link's id and keeps the saved pick.
    func testABoxWithoutProfilesSettlesWithoutAPicker() {
        XCTAssertEqual(after(nil, saved: annaPick), .settle(id: nil, remember: annaPick))
        XCTAssertEqual(after(nil, link: "ben"), .settle(id: "ben", remember: nil))
    }

    /// One profile, or a saved pick the box still lists, dials without the picker.
    func testOneProfileOrAListedPickSettles() {
        XCTAssertEqual(after([ben]), .settle(id: "ben", remember: nil))
        XCTAssertEqual(after([anna, ben], saved: annaPick), .settle(id: "anna", remember: annaPick))
    }

    /// Several profiles and no usable pick show the picker, naming a pick the box dropped.
    func testSeveralProfilesWithoutAPickShowThePicker() {
        XCTAssertEqual(after([anna, ben]), .pick([anna, ben], saved: nil, gone: nil))
        let gone = ProfilePick(id: "carl", displayName: "Carl")
        XCTAssertEqual(after([anna, ben], saved: gone), .pick([anna, ben], saved: nil, gone: "Carl"))
    }

    /// A link naming no listed profile shows the picker with the saved pick marked.
    func testAnUnknownLinkProfileShowsThePickerAtTheSavedPick() {
        XCTAssertEqual(
            after([anna, ben], saved: annaPick, link: "zoe"),
            .pick([anna, ben], saved: "anna", gone: nil))
    }
}
