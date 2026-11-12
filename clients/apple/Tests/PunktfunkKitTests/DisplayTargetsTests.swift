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
        XCTAssertEqual(t.cardLine, "Streams LG TV")
        XCTAssertTrue(t.mayChoose, "a host without the grant lets every device choose")
        XCTAssertNil(t.role)
    }

    func testYoursAbsentOrNullFollowsTheHost() throws {
        for body in [
            #"{"host_default": {"monitor": "DP-1"}, "targets": []}"#,
            #"{"host_default": {"monitor": "DP-1"}, "yours": null}"#,
        ] {
            let t = try decode(body)
            XCTAssertNil(t.yours)
            XCTAssertEqual(t.current, .monitor("DP-1"))
            XCTAssertEqual(t.cardLine, "Streams DP-1", "an unlisted head names its connector")
        }
        XCTAssertNil(try decode("{}").cardLine, "the own screen says nothing")
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
        XCTAssertEqual(try json(ScreenPick(screen: .own)), #"{"screen":"own"}"#)
        XCTAssertEqual(
            try json(ScreenPick(screen: .own, topology: "extend")),
            #"{"screen":"own","topology":"extend"}"#)
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

    /// The console words the rows itself, so `may_choose` and `role` reach it in the host's keys.
    func testTheConsoleRowCarriesTheGrantAndTheRole() throws {
        var host = StoredHost(name: "Desk", address: "192.168.1.10")
        host.pinnedSHA256 = Data([0xab, 0xcd])
        let answer = try decode("""
            {"may_choose": false, "host_default": "own", "yours": "own", "targets": [],
             "role": {"yours": "second", "host_default": "only", "offered": ["only", "second"]}}
            """)
        let json = ConsoleJSON.hostRows(
            saved: [host], discovered: [], online: [host.id], presets: [],
            screens: ["abcd": answer])
        let rows = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [[String: Any]])
        let screens = try XCTUnwrap(rows.first?["screens"] as? [String: Any])
        XCTAssertEqual(screens["may_choose"] as? Bool, false)
        let role = try XCTUnwrap(screens["role"] as? [String: Any])
        XCTAssertEqual(role["yours"] as? String, "second")
        XCTAssertEqual(role["host_default"] as? String, "only")
        XCTAssertEqual(role["offered"] as? [String], ["only", "second"])
    }

    /// A console `SetScreen` writes its `screen`, and a role's `topology` beside it.
    func testASetScreenCommandWritesItsTopology() throws {
        let doc = try SharedVector.load("console-bridge-vectors.json")
        let commands = try XCTUnwrap(doc["commands"] as? [Any])
        let sample = try XCTUnwrap(
            commands.lazy.compactMap { ($0 as? [String: Any])?["SetScreen"] as? [String: Any] }
                .first)
        let pick = try XCTUnwrap(ConsoleJSON.screenPick(sample))
        XCTAssertEqual(pick, ScreenPick(screen: .own, topology: "extend"))
        XCTAssertEqual(
            ConsoleJSON.screenPick(["screen": ["monitor": "HDMI-A-1"], "label": "LG TV"]),
            ScreenPick(screen: .monitor("HDMI-A-1")))
    }

    /// `clients/shared/screen-rows-vectors.json`: each answer's rows, note and card line, as the
    /// Rust `display_targets` and the Kotlin copy draw them.
    func testSharedVectors() throws {
        let file = try SharedVector.load("screen-rows-vectors.json")
        let cases = try XCTUnwrap(file["cases"] as? [[String: Any]])
        XCTAssertFalse(cases.isEmpty)
        for c in cases {
            let name = try XCTUnwrap(c["name"] as? String)
            let body = try JSONSerialization.data(withJSONObject: try XCTUnwrap(c["targets"]))
            let t = try JSONDecoder().decode(DisplayTargets.self, from: body)
            let want = try XCTUnwrap(c["rows"] as? [[String: Any]], name)
            let rows = t.rows
            XCTAssertEqual(rows.count, want.count, name)
            for (row, w) in zip(rows, want) {
                let at = "\(name): \(row.id)"
                XCTAssertEqual(kind(row.kind), w["kind"] as? String, at)
                XCTAssertEqual(row.id, w["id"] as? String, at)
                XCTAssertEqual(row.label, try text(w["label"]), at)
                XCTAssertEqual(row.hint, try text(w["hint"]), at)
                XCTAssertEqual(row.detail, w["detail"] as? String, at)
                XCTAssertEqual(row.header, try text(w["header"]), at)
                XCTAssertEqual(row.checked, w["checked"] as? Bool, at)
                XCTAssertEqual(row.pickable, w["pickable"] as? Bool, at)
                XCTAssertEqual(row.reason, try text(w["reason"]), at)
                XCTAssertEqual(try put(row.pick), w["put"] as? NSObject, at)
            }
            XCTAssertEqual(t.note, try text(c["note"]), name)
            XCTAssertEqual(t.cardLine, try text(c["card"]), name)
        }
    }

    private func kind(_ kind: ScreenRow.Kind) -> String {
        switch kind {
        case .role: "role"
        case .monitor: "monitor"
        case .own: "own"
        }
    }

    /// A pick as the PUT body's JSON, or null for a row that writes nothing.
    private func put(_ pick: ScreenPick?) throws -> NSObject? {
        guard let pick else { return NSNull() }
        return try JSONSerialization.jsonObject(with: JSONEncoder().encode(pick)) as? NSObject
    }

    /// A vector's text slot as this build words it: null, the host's own string, or the client
    /// message its key names, placeholders filled by name.
    private func text(_ slot: Any?) throws -> String? {
        if let host = slot as? String { return host }
        guard let message = slot as? [String: Any] else { return nil }
        let key = try XCTUnwrap(message["key"] as? String)
        let args = message["args"] as? [String: String] ?? [:]
        switch key {
        case "screen_role_only": return String(localized: L10n.screenRoleOnly)
        case "screen_role_main": return String(localized: L10n.screenRoleMain)
        case "screen_role_second": return String(localized: L10n.screenRoleSecond)
        case "screen_role_only_hint": return String(localized: L10n.screenRoleOnlyHint)
        case "screen_role_main_hint": return String(localized: L10n.screenRoleMainHint)
        case "screen_role_second_hint": return String(localized: L10n.screenRoleSecondHint)
        case "screen_show_monitor": return String(localized: L10n.screenShowMonitor)
        case "screen_host_decides": return String(localized: L10n.screenHostDecides)
        case "screen_locked_hint": return String(localized: L10n.screenLockedHint)
        case "screen_own": return String(localized: L10n.screenOwn)
        case "screen_off": return String(localized: L10n.screenOff)
        case "screen_in_use": return L10n.screenInUse(device: try XCTUnwrap(args["device"]))
        case "screen_streams": return L10n.screenStreams(screen: try XCTUnwrap(args["screen"]))
        default:
            XCTFail("no message \(key) in this test's table")
            return nil
        }
    }
}
