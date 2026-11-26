// HostOffers.resolve: a row per gate a host card applies to its acts.

import XCTest

@testable import PunktfunkKit

final class HostOffersTests: XCTestCase {
    private func host(paired: Bool = true, pick: Bool = false, remember: Bool = true) -> StoredHost {
        var host = StoredHost(name: "Desk", address: "10.0.0.2")
        if paired { host.pinnedSHA256 = Data(repeating: 0xAB, count: 32) }
        if pick { host.pickedProfile = ProfilePick(id: "p1", displayName: "Ada", remember: remember) }
        return host
    }

    private func offers(
        _ host: StoredHost, pinned: Bool = false, online: Bool = true, isDemo: Bool = false,
        canWake: Bool = false, profiles: Int = 0
    ) -> HostOffers {
        HostOffers.resolve(
            host: host, pinned: pinned, online: online, isDemo: isDemo, canWake: canWake,
            profiles: profiles)
    }

    func testAPairedHostOffersItsPageLibraryAndDiagnostics() {
        XCTAssertEqual(offers(host()), [.browse, .speedTest, .sendLogs, .details, .power])
    }

    /// Library, speed test and logs dial with the pinned identity.
    func testAnUnpairedHostWaitsForAPairing() {
        XCTAssertEqual(offers(host(paired: false)), [.details, .power])
    }

    func testAWakeNeedsAnOfflineHostItCanReach() {
        XCTAssertTrue(offers(host(), online: false, canWake: true).contains(.wake))
        XCTAssertFalse(offers(host(), online: true, canWake: true).contains(.wake))
        XCTAssertFalse(offers(host(), online: false, canWake: false).contains(.wake))
        XCTAssertTrue(offers(host(paired: false), online: false, canWake: true).contains(.wake))
    }

    /// As on the console: a remembered pick, or two profiles to choose from.
    func testSwitchProfileNeedsAChoiceOnAPairedHost() {
        XCTAssertTrue(offers(host(pick: true)).contains(.switchProfile))
        XCTAssertTrue(offers(host(), profiles: 2).contains(.switchProfile))
        XCTAssertFalse(offers(host()).contains(.switchProfile))
        XCTAssertFalse(offers(host(), profiles: 1).contains(.switchProfile))
        XCTAssertFalse(offers(host(pick: true, remember: false), profiles: 1).contains(.switchProfile))
        XCTAssertFalse(offers(host(paired: false, pick: true)).contains(.switchProfile))
    }

    func testTheDemoHostHasNoSpeedTestLogsOrProfiles() {
        XCTAssertEqual(offers(host(pick: true), isDemo: true), [.browse, .details, .power])
    }

    /// A pinned card is a shortcut: no page, power, wake or profile switch of its own.
    func testAPinnedCardCarriesNoHostActs() {
        XCTAssertEqual(
            offers(host(pick: true), pinned: true, online: false, canWake: true),
            [.browse, .speedTest, .sendLogs])
        XCTAssertEqual(offers(host(paired: false), pinned: true), [])
    }

    func testAnUnavailableActionIsExplainedInTheHostsWords() {
        let busy = HostAction(
            id: "power.sleep", title: "Sleep", danger: true, available: false,
            unavailableReason: "Another device is streaming")
        XCTAssertEqual(HostOffers.route(action: busy), .explain("Another device is streaming"))
    }

    func testAnUnavailableActionWithoutAReasonGetsTheAppsSentence() {
        let action = HostAction(id: "power.sleep", title: "Sleep", available: false)
        XCTAssertEqual(
            HostOffers.route(action: action),
            .explain(L10n.hostcardActionUnavailableBody(action: action.label)))
    }

    func testADestructiveActionAsksFirst() {
        let restart = HostAction(id: "power.reboot", title: "Restart", danger: true)
        XCTAssertEqual(HostOffers.route(action: restart), .confirm)
        XCTAssertEqual(HostOffers.route(action: HostAction(id: "power.sleep", title: "Sleep")), .run)
    }
}
