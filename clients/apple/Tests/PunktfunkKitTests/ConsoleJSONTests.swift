import XCTest

@testable import PunktfunkKit
import PunktfunkShared

/// What this client hands the console. The shapes are the Rust model's, so the assertions are
/// about the keys those structs deserialize — a renamed key here is a row that silently empties.
final class ConsoleJSONTests: XCTestCase {
    private func host(name: String, paired: Bool = true) -> StoredHost {
        var h = StoredHost(name: name, address: "192.168.1.10")
        h.port = 9777
        h.pinnedSHA256 = paired ? Data([0xab, 0xcd]) : nil
        h.macAddresses = ["aa:bb:cc:dd:ee:ff"]
        h.osChain = "linux/fedora"
        h.lastConnected = Date(timeIntervalSince1970: 1_700_000_000)
        return h
    }

    private func rows(_ json: String) throws -> [[String: Any]] {
        let data = try XCTUnwrap(json.data(using: .utf8))
        return try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [[String: Any]])
    }

    /// A paired host, its pinned preset card and a discovered stranger, in carousel order.
    func testHostRowsCarryTheModelsKeys() throws {
        var saved = host(name: "Desk")
        saved.pinnedPresetIDs = ["p1"]
        let preset = StreamPreset(name: "Couch", id: "p1")
        let json = ConsoleJSON.hostRows(
            saved: [saved], discovered: [], online: [saved.id], presets: [preset])
        let rows = try rows(json)
        XCTAssertEqual(rows.count, 2, "the host and its pinned card")

        let row = rows[0]
        XCTAssertEqual(row["key"] as? String, "abcd", "a paired row is keyed by fingerprint")
        XCTAssertEqual(row["id"] as? String, saved.id.uuidString)
        XCTAssertEqual(row["name"] as? String, "Desk")
        XCTAssertEqual(row["port"] as? Int, 9777)
        XCTAssertEqual(row["paired"] as? Bool, true)
        XCTAssertEqual(row["online"] as? Bool, true)
        XCTAssertEqual(row["can_wake"] as? Bool, false, "an online host is not woken")
        XCTAssertEqual(row["os"] as? String, "linux/fedora")
        XCTAssertEqual(row["last_used"] as? Int, 1_700_000_000)
        XCTAssertTrue(row["pin"] is NSNull, "the primary tile carries no pin")

        let card = rows[1]
        XCTAssertEqual(
            card["key"] as? String, "abcd\u{0}p1", "a pinned card's key rides the preset id")
        XCTAssertEqual((card["pin"] as? [String: Any])?["name"] as? String, "Couch")
        XCTAssertTrue(card["bound_preset"] is NSNull)
    }

    /// An unpaired host that is offline and has a MAC offers Wake, and is keyed by address.
    func testAnOfflineHostOffersWake() throws {
        let saved = host(name: "Attic", paired: false)
        let row = try XCTUnwrap(
            rows(ConsoleJSON.hostRows(saved: [saved], discovered: [], online: [], presets: []))
                .first)
        XCTAssertEqual(row["key"] as? String, "192.168.1.10:9777")
        XCTAssertEqual(row["paired"] as? Bool, false)
        XCTAssertEqual(row["can_wake"] as? Bool, true)
    }

    func testKnownHostsCarryWhatALinkNeeds() throws {
        let saved = host(name: "Desk")
        let data = try XCTUnwrap(ConsoleJSON.knownHosts([saved]).data(using: .utf8))
        let doc = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let entry = try XCTUnwrap((doc["hosts"] as? [[String: Any]])?.first)
        XCTAssertEqual(entry["fp_hex"] as? String, "abcd")
        XCTAssertEqual(entry["mac"] as? [String], ["aa:bb:cc:dd:ee:ff"])
        XCTAssertEqual(entry["id"] as? String, saved.id.uuidString)
    }

    /// The settings document: the app's keys over the console's own, and the two that are
    /// stored as wire integers here but named in the console.
    func testSettingsDocumentCarriesBothSides() throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: "console-json-tests"))
        defaults.removePersistentDomain(forName: "console-json-tests")
        // A key only the console owns (this client has no decoder pick), from an earlier save.
        defaults.set(["decoder": "native-vulkan"], forKey: ConsoleSettings.documentKey)
        defaults.set(2, forKey: DefaultsKey.compositor)
        defaults.set(48_000, forKey: DefaultsKey.bitrateKbps)
        defaults.set("off", forKey: DefaultsKey.statsVerbosity)

        let doc = ConsoleSettings.document(defaults)
        XCTAssertEqual(
            doc["decoder"] as? String, "native-vulkan", "the console's own key survives")
        XCTAssertEqual(doc["bitrate_kbps"] as? Int, 48_000)
        XCTAssertEqual(doc["compositor"] as? String, "wlroots", "the wire integer is named")
        XCTAssertEqual(doc["show_stats"] as? Bool, false, "derived from the tier")
        XCTAssertEqual(doc["hdr_enabled"] as? Bool, true, "an unwritten key reads as the app does")

        ConsoleSettings.apply(
            ["bitrate_kbps": 20_000, "compositor": "gamescope", "vsync": true], defaults)
        XCTAssertEqual(defaults.integer(forKey: DefaultsKey.bitrateKbps), 20_000)
        XCTAssertEqual(defaults.integer(forKey: DefaultsKey.compositor), 4)
        XCTAssertTrue(defaults.bool(forKey: DefaultsKey.vsync))
        defaults.removePersistentDomain(forName: "console-json-tests")
    }
}
