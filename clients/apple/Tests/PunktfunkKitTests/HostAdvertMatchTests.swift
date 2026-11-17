// Which saved host a live advert belongs to. The saved list and the discovered section are two
// halves of one screen, so a wrong answer here is visible twice: a host that reads Offline while
// it advertises, or one that never appears to be added at all.

import PunktfunkKit
import XCTest

@MainActor
final class HostAdvertMatchTests: XCTestCase {
    private let windows = String(repeating: "ab", count: 32)
    private let linux = String(repeating: "cd", count: 32)

    private func advert(fp: String?, host: String = "192.168.1.9") -> DiscoveredHost {
        HostDiscovery.debugAdvert(id: "id-\(fp ?? "none")", name: "Desk", host: host, fingerprintHex: fp)
    }

    /// A dual-boot box: one lease, one MAC, a certificate per OS. The OS that is up must not read
    /// as the one already saved, or it stays out of the discovered section and can never be added.
    func testASecondOsAtOneAddressIsADifferentHost() {
        let other = advert(fp: linux)
        XCTAssertFalse(other.matches(pin: windows, address: "192.168.1.9", port: 9777))
    }

    /// The pin decides on its own, so a host that took a new DHCP lease is still itself — and
    /// case is not part of the answer.
    func testTheSameHostMatchesOnItsPinAcrossAnAddressChange() {
        let moved = advert(fp: windows.uppercased(), host: "192.168.1.20")
        XCTAssertTrue(moved.matches(pin: windows, address: "192.168.1.9", port: 9777))
    }

    /// With one side unpinned there is nothing but the address to go on: a host saved by hand,
    /// or an advert from a host too old to carry `fp`.
    func testAnUnpinnedSideFallsBackToTheAddress() {
        XCTAssertTrue(advert(fp: nil).matches(pin: windows, address: "192.168.1.9", port: 9777))
        XCTAssertTrue(advert(fp: linux).matches(pin: nil, address: "192.168.1.9", port: 9777))
        XCTAssertFalse(advert(fp: nil).matches(pin: nil, address: "192.168.1.9", port: 9778))
        XCTAssertFalse(advert(fp: nil).matches(pin: nil, address: "192.168.1.10", port: 9777))
    }
}
