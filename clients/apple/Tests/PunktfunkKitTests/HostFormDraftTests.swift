// The add-or-edit-host form's rules, which three screens used to answer three different ways.

import XCTest

@testable import PunktfunkShared

final class HostFormDraftTests: XCTestCase {
    func testBlankPortMeansTheDefault() {
        let draft = HostFormDraft(address: "nuc.local")
        XCTAssertEqual(draft.resolvedPort, HostFormDraft.defaultPort)
        XCTAssertTrue(draft.canSave)
    }

    func testEmptyAddressIsTheFirstThingToFix() {
        XCTAssertEqual(HostFormDraft(address: "   ").problem, .emptyAddress)
    }

    func testPortOutOfRangeIsRefusedRatherThanClamped() {
        // 65535 clamped, or silently replaced by the default, are the two behaviours that used to
        // save a different record than the user typed.
        XCTAssertEqual(HostFormDraft(address: "nuc.local", port: "99999").problem, .badPort)
        XCTAssertEqual(HostFormDraft(address: "nuc.local", port: "0").problem, .badPort)
        XCTAssertEqual(HostFormDraft(address: "nuc.local", port: "no").problem, .badPort)
    }

    func testPortInsideTheAddressIsSplitOut() {
        // What the host cards render, so it is what a user pastes back in.
        let draft = HostFormDraft(address: " 192.168.1.5:47989 ")
        XCTAssertEqual(draft.trimmedAddress, "192.168.1.5")
        XCTAssertEqual(draft.resolvedPort, 47989)
        XCTAssertTrue(draft.canSave)
    }

    func testBareIPv6KeepsAllItsColons() {
        let draft = HostFormDraft(address: "fd7a:115c::1")
        XCTAssertEqual(draft.trimmedAddress, "fd7a:115c::1")
        XCTAssertEqual(draft.resolvedPort, HostFormDraft.defaultPort)
    }

    func testBracketedIPv6WithAPortSplits() {
        let draft = HostFormDraft(address: "[fd7a:115c::1]:9777")
        XCTAssertEqual(draft.trimmedAddress, "[fd7a:115c::1]")
        XCTAssertEqual(draft.resolvedPort, 9777)
    }

    func testApplyLeavesEverythingTheFormDoesNotShow() {
        var host = StoredHost(name: "Old", address: "10.0.0.1", port: 1234)
        host.pinnedSHA256 = Data(repeating: 0xAB, count: 32)
        host.macAddresses = ["aa:bb:cc:dd:ee:ff"]
        HostFormDraft(name: " Desk ", address: "10.0.0.2", port: "9777").apply(to: &host)
        XCTAssertEqual(host.name, "Desk")
        XCTAssertEqual(host.address, "10.0.0.2")
        XCTAssertEqual(host.port, 9777)
        XCTAssertEqual(host.pinnedSHA256?.count, 32, "a rename must not drop the pin")
        XCTAssertEqual(host.wakeMacs, ["aa:bb:cc:dd:ee:ff"])
    }

    /// The Wake-on-LAN field takes a MAC as Windows prints it, and only hex octets.
    func testMacsParseInBothSpellings() {
        XCTAssertEqual(HostFormDraft.parseMacs("AA-BB-CC-DD-EE-FF"), ["aa:bb:cc:dd:ee:ff"])
        XCTAssertEqual(
            HostFormDraft.parseMacs("aa:bb:cc:dd:ee:ff, 11-22-33-44-55-66"),
            ["aa:bb:cc:dd:ee:ff", "11:22:33:44:55:66"])
        XCTAssertNil(HostFormDraft.parseMacs("+f:+f:+f:+f:+f:+f"))
        XCTAssertNil(HostFormDraft.parseMacs("aa:bb:cc:dd:ee"))
        XCTAssertNil(HostFormDraft.parseMacs(""))
    }

    private static let record = StoredHost(
        name: "Desk", address: "10.0.0.1", port: 9777, macAddresses: ["aa:bb:cc:dd:ee:ff"])
    private static let loaded = HostFormDraft(record: record)

    /// The record as the sweep and an advert leave it while a field has focus.
    private static var moved: StoredHost {
        var host = record
        host.move(to: "10.0.0.9", port: 9800)
        host.macAddresses = ["11:22:33:44:55:66"]
        return host
    }

    func testAFormLoadsARecordAsItShowsIt() {
        XCTAssertEqual(
            Self.loaded,
            HostFormDraft(name: "Desk", address: "10.0.0.1", port: "9777", macs: "aa:bb:cc:dd:ee:ff"))
    }

    func testASaveWritesEachChangedField() {
        var host = Self.record
        HostFormDraft(name: "Den", address: "10.0.0.2:5000", port: "9777", macs: "11-22-33-44-55-66")
            .commit(to: &host, changedFrom: Self.loaded)
        XCTAssertEqual(host.name, "Den")
        XCTAssertEqual(host.address, "10.0.0.2")
        XCTAssertEqual(host.port, 5000)
        XCTAssertEqual(host.wakeMacs, ["11:22:33:44:55:66"])
    }

    /// Name, address and port go in together or not at all.
    func testABadPortKeepsTheRecordsNameAddressAndPort() {
        var host = Self.record
        var form = Self.loaded
        form.name = "Den"
        form.port = "99999"
        form.commit(to: &host, changedFrom: Self.loaded)
        XCTAssertEqual(host, Self.record)
    }

    func testUnreadableMacsKeepTheRecordsAndABlankFieldClearsThem() {
        var host = Self.record
        var form = Self.loaded
        form.macs = "not a mac"
        form.commit(to: &host, changedFrom: Self.loaded)
        XCTAssertEqual(host.wakeMacs, ["aa:bb:cc:dd:ee:ff"])
        form.macs = "  "
        form.commit(to: &host, changedFrom: Self.loaded)
        XCTAssertNil(host.macAddresses)
    }

    /// R4-4: the record moved under untouched fields, and a save writes nothing back.
    func testASaveWithNothingChangedKeepsWhatTheRecordMovedTo() {
        var host = Self.moved
        Self.loaded.commit(to: &host, changedFrom: Self.loaded)
        XCTAssertEqual(host, Self.moved)
    }

    /// R4-4: each field the player changed goes in; the rest keep what the record moved to.
    func testEachChangedFieldGoesInAlone() {
        let edits: [(WritableKeyPath<HostFormDraft, String>, String, (inout StoredHost) -> Void)] = [
            (\.name, "Den", { $0.name = "Den" }),
            (\.address, "10.0.0.7", { $0.address = "10.0.0.7" }),
            (\.port, "5000", { $0.port = 5000 }),
            (\.macs, "22-33-44-55-66-77", { $0.macAddresses = ["22:33:44:55:66:77"] }),
        ]
        for (field, typed, expect) in edits {
            var form = Self.loaded
            form[keyPath: field] = typed
            var host = Self.moved
            form.commit(to: &host, changedFrom: Self.loaded)
            var want = Self.moved
            expect(&want)
            XCTAssertEqual(host, want, "\(typed)")
        }
    }
}
