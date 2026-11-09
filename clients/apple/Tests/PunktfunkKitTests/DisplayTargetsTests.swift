import XCTest

@testable import PunktfunkKit

/// `GET /display/targets` and the pick's write, in the host's JSON: the same shapes the Rust
/// `pf-client-core::display_targets` tests pin.
final class DisplayTargetsTests: XCTestCase {
    private let plan = """
        { "locked": false, "host_default": "own", "yours": {"monitor": "HDMI-A-1"},
          "targets": [
            { "id": "own", "label": "Own screen" },
            { "id": "HDMI-A-1", "label": "LG TV", "connector": "HDMI-A-1", "mode": "3840x2160@60",
              "primary": true, "enabled": true, "in_use_by": null },
            { "id": "DP-2", "label": "DELL U2723QE", "connector": "DP-2", "mode": "3840x2160@60",
              "primary": false, "enabled": false, "in_use_by": null } ] }
        """

    private func decode(_ json: String) throws -> DisplayTargets {
        try JSONDecoder().decode(DisplayTargets.self, from: Data(json.utf8))
    }

    func testThePlansBodyDecodes() throws {
        let t = try decode(plan)
        XCTAssertFalse(t.locked)
        XCTAssertEqual(t.hostDefault, .own)
        XCTAssertEqual(t.yours, .monitor("HDMI-A-1"))
        XCTAssertEqual(t.current, .monitor("HDMI-A-1"))
        let own = t.targets[0]
        XCTAssertEqual(own.target, .own)
        XCTAssertTrue(own.enabled, "an own row without the field is pickable")
        XCTAssertEqual(own.name, "Own screen")
        XCTAssertNil(own.reason)
        XCTAssertEqual(t.targets[1].name, "LG TV")
        XCTAssertTrue(t.targets[1].primary)
        XCTAssertEqual(t.targets[1].detail, "HDMI-A-1 \u{b7} 3840x2160@60")
        XCTAssertEqual(t.targets[2].reason, "Off")
        XCTAssertEqual(t.summary, "Streams LG TV")
    }

    func testYoursAbsentOrNullFollowsTheHost() throws {
        for body in [
            #"{"host_default": {"monitor": "DP-1"}, "targets": []}"#,
            #"{"host_default": {"monitor": "DP-1"}, "yours": null}"#,
        ] {
            let t = try decode(body)
            XCTAssertNil(t.yours)
            XCTAssertEqual(t.current, .monitor("DP-1"))
            XCTAssertEqual(t.summary, "Streams DP-1", "an unlisted head names its connector")
        }
        XCTAssertEqual(try decode("{}").summary, "Own screen")
    }

    func testATakenHeadSaysWhoHasIt() throws {
        let row = try JSONDecoder().decode(ScreenTarget.self, from: Data("""
            {"id": "HDMI-A-1", "label": "HDMI-A-1", "connector": "HDMI-A-1",
             "mode": "1920x1080@60", "in_use_by": "Ben's Apple TV"}
            """.utf8))
        XCTAssertEqual(row.reason, "Streamed by Ben's Apple TV")
        XCTAssertEqual(row.detail, "1920x1080@60", "the label already names the connector")
    }

    func testATargetEncodesAsTheHostSpellsIt() throws {
        func json(_ value: some Encodable) throws -> String {
            let encoder = JSONEncoder()
            encoder.outputFormatting = .sortedKeys
            return String(decoding: try encoder.encode(value), as: UTF8.self)
        }
        XCTAssertEqual(try json(DisplayTarget.own), #""own""#)
        XCTAssertEqual(try json(DisplayTarget.monitor("HDMI-A-1")), #"{"monitor":"HDMI-A-1"}"#)
        XCTAssertEqual(try json(["screen": DisplayTarget.own]), #"{"screen":"own"}"#)
        XCTAssertThrowsError(
            try JSONDecoder().decode(DisplayTarget.self, from: Data(#""elsewhere""#.utf8)))
    }

    /// The console's host row carries the host's body as `screens`, as `HostRow` reads it.
    func testTheConsoleRowCarriesTheScreens() throws {
        var host = StoredHost(name: "Desk", address: "192.168.1.10")
        host.pinnedSHA256 = Data([0xab, 0xcd])
        let json = ConsoleJSON.hostRows(
            saved: [host], discovered: [], online: [host.id], presets: [],
            screens: ["abcd": try decode(plan)])
        let rows = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [[String: Any]])
        let screens = try XCTUnwrap(rows.first?["screens"] as? [String: Any])
        XCTAssertEqual(screens["host_default"] as? String, "own")
        XCTAssertEqual((screens["yours"] as? [String: Any])?["monitor"] as? String, "HDMI-A-1")
        let targets = try XCTUnwrap(screens["targets"] as? [[String: Any]])
        XCTAssertEqual(targets.count, 3)
        XCTAssertEqual(targets[2]["enabled"] as? Bool, false)

        let bare = ConsoleJSON.hostRows(saved: [host], discovered: [], online: [], presets: [])
        let row = try XCTUnwrap(
            (JSONSerialization.jsonObject(with: Data(bare.utf8)) as? [[String: Any]])?.first)
        XCTAssertTrue(row["screens"] is NSNull, "no answer yet: no Screen section")
    }
}
