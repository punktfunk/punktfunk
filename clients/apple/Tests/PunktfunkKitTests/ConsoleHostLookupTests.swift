// ConsoleJSON.host: which saved record a console command means. The fingerprint keys a host,
// never the address: two systems on one box share an address and a port.

import Foundation
import XCTest

@testable import PunktfunkKit
import PunktfunkShared

final class ConsoleHostLookupTests: XCTestCase {
    private func host(_ name: String, _ address: String, pin: [UInt8]?) -> StoredHost {
        var h = StoredHost(name: name, address: address)
        h.port = 9777
        h.pinnedSHA256 = pin.map { Data($0) }
        return h
    }

    private var dualBoot: [StoredHost] {
        [host("windows", "10.0.0.5", pin: [0xaa]), host("linux", "10.0.0.5", pin: [0xbb])]
    }

    /// A command for the second system of a dual-boot box gets that system's record.
    func testAFingerprintOutranksTheSharedAddress() {
        let found = ConsoleJSON.host(in: dualBoot, fp: "bb", addr: "10.0.0.5", port: 9777)
        XCTAssertEqual(found?.name, "linux")
    }

    /// A fingerprint no record pins falls back to the unpaired record there, never a pinned one.
    func testAnUnknownFingerprintNeverBorrowsAnotherPin() {
        XCTAssertNil(ConsoleJSON.host(in: dualBoot, fp: "cc", addr: "10.0.0.5", port: 9777))
        let hosts = dualBoot + [host("fresh", "10.0.0.5", pin: nil)]
        let found = ConsoleJSON.host(in: hosts, fp: "cc", addr: "10.0.0.5", port: 9777)
        XCTAssertEqual(found?.name, "fresh")
    }

    /// A pairing lands on the record pinned to its certificate, else the unpaired one at the
    /// address, else a new one: never the other system of a dual-boot box.
    func testAPairingLandsOnItsOwnSystemsRecord() {
        let hosts = dualBoot + [host("fresh", "10.0.0.5", pin: nil)]
        XCTAssertEqual(ConsoleJSON.host(in: hosts, fp: "bb", addr: "10.0.0.5", port: 9777)?.name, "linux")
        XCTAssertEqual(ConsoleJSON.host(in: hosts, fp: "cc", addr: "10.0.0.5", port: 9777)?.name, "fresh")
        XCTAssertNil(ConsoleJSON.host(in: dualBoot, fp: "cc", addr: "10.0.0.5", port: 9777))
    }

    /// With no fingerprint the address is all there is.
    func testNoFingerprintMatchesByAddress() {
        let found = ConsoleJSON.host(in: dualBoot, fp: "", addr: "10.0.0.5", port: 9777)
        XCTAssertEqual(found?.name, "windows")
    }

    /// An empty key names nothing; before, it matched every unpaired record's empty fingerprint.
    func testAnEmptyKeyNamesNoHost() {
        let hosts = [host("fresh", "10.0.0.9", pin: nil)]
        XCTAssertNil(ConsoleJSON.host(in: hosts, key: ""))
        XCTAssertNil(ConsoleJSON.host(in: hosts, key: "\u{0}preset"))
    }

    /// An unpaired row's key is `addr:port`; the port is behind the LAST colon.
    func testAnAddressKeySplitsAtItsLastColon() {
        let hosts = [host("v6", "fe80::1", pin: nil), host("v4", "10.0.0.9", pin: nil)]
        XCTAssertEqual(ConsoleJSON.host(in: hosts, key: "fe80::1:9777")?.name, "v6")
        XCTAssertEqual(ConsoleJSON.host(in: hosts, key: "10.0.0.9:9777")?.name, "v4")
        XCTAssertEqual(ConsoleJSON.host(in: dualBoot, key: "bb\u{0}preset")?.name, "linux")
    }

    /// R5-5: a pinned card's key names its preset, so a wake from that card connects with it.
    func testAPinnedCardsKeyNamesItsPreset() {
        XCTAssertEqual(ConsoleJSON.preset(ofKey: "bb\u{0}p1"), "p1")
        XCTAssertEqual(ConsoleJSON.preset(ofKey: "fe80::1:9777\u{0}p1"), "p1")
        XCTAssertNil(ConsoleJSON.preset(ofKey: "bb"))
        XCTAssertNil(ConsoleJSON.preset(ofKey: "bb\u{0}"))
        XCTAssertNil(ConsoleJSON.preset(ofKey: ""))
    }
}
