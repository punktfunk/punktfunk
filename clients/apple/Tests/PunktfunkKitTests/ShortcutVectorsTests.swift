// Replays `clients/shared/shortcut-vectors.json`, which the Rust and Kotlin ports replay too, so
// the shortcut rules cannot drift. A new case belongs in that file; each section is read the way
// pf-client-core's `shortcuts/tests.rs` reads it.

import XCTest
@testable import PunktfunkKit
@testable import PunktfunkShared

final class ShortcutVectorsTests: XCTestCase {
    private typealias S = Shortcuts

    /// Read from the repo, not a bundle copy: four levels up from this file is `clients/`.
    private func section(_ name: String) throws -> [[String: Any]] {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<4 { url.deleteLastPathComponent() }
        url.appendPathComponent("shared/shortcut-vectors.json")
        let file = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let cases = try XCTUnwrap((file[name] as? [String: Any])?["cases"] as? [[String: Any]], name)
        XCTAssertFalse(cases.isEmpty, name)
        return cases
    }

    private func json(_ text: String) throws -> NSDictionary {
        try XCTUnwrap(JSONSerialization.jsonObject(with: Data(text.utf8)) as? NSDictionary)
    }

    private func blob(_ bindings: Any?) throws -> String {
        let data = try JSONSerialization.data(withJSONObject: ["v": 1, "bindings": bindings ?? []])
        return String(decoding: data, as: UTF8.self)
    }

    private func client(_ name: String) throws -> S.Client {
        let all: [String: S.Client] = [
            "desktop": .desktop, "android": .android, "apple": .apple, "mac": .mac,
            "webos": .webOS, "web": .web, "tizen": .tizen, "ps5": .ps5, "switch": .switch,
        ]
        return try XCTUnwrap(all[name], name)
    }

    func testParseVectors() throws {
        let marker = S(bindings: [.init(trigger: .pad(buttons: [0x8000], hold: true), action: .ring)])
        for c in try section("parse") {
            let name = c["name"] as? String ?? "?"
            let raw = try XCTUnwrap(c["blob"] as? String, name)
            XCTAssertEqual(try json(S.parse(raw).toJSON()), c["expect"] as? NSDictionary, name)
            let resolved = S.read(raw) ?? marker
            XCTAssertEqual(resolved == marker, c["defaults"] as? Bool, "\(name): defaults")
        }
    }

    func testDefaultsVectors() throws {
        for c in try section("defaults") {
            let who = try client(c["client"] as? String ?? "")
            let gameMode = try XCTUnwrap(c["game_mode"] as? Bool)
            let legacy = try XCTUnwrap(c["guide_gesture"] as? String)
            let got = S.defaults(client: who, gameMode: gameMode, legacyGuideGesture: legacy)
            XCTAssertEqual(try json(got.toJSON())["bindings"] as? NSArray, c["expect"] as? NSArray, "\(c)")
            XCTAssertEqual(S.resolve("", client: who, gameMode: gameMode, legacyGuideGesture: legacy), got)
        }
    }

    func testValidateVectors() throws {
        for c in try section("validate") {
            let name = c["name"] as? String ?? "?"
            let list = try XCTUnwrap(c["bindings"] as? [Any], name)
            let have = S.parse(try blob(list))
            XCTAssertEqual(have.bindings.count, list.count, name)
            let candidate = try XCTUnwrap(S.parse(try blob([c["candidate"]])).bindings.first, name)
            let index = c["index"] as? Int ?? -1
            let want: S.Conflict?
            switch c["expect"] as? String {
            case "ok": want = nil
            case "identical": want = .identical(index)
            case "press_subset": want = .pressSubset(index)
            case "single_press": want = .singlePress
            case "no_modifier": want = .noModifier
            case "no_key": want = .noKey
            case let other: return XCTFail("\(name): an expect the vectors use: \(String(describing: other))")
            }
            XCTAssertEqual(have.validate(candidate), want, name)
        }
    }

    func testEngineVectors() throws {
        for c in try section("engine") {
            let name = c["name"] as? String ?? "?"
            let device: S.Device = c["device"] as? String == "key" ? .keyboard : .pad
            func id(_ v: Any?) throws -> UInt32 {
                let n = try XCTUnwrap(v as? String, name)
                return try XCTUnwrap(device == .pad ? S.buttonBit(n) : keyVk(n), "\(name): a name: \(n)")
            }
            let list = try XCTUnwrap(c["bindings"] as? [Any], name)
            let bindings = S.parse(try blob(list)).bindings
            XCTAssertEqual(bindings.count, list.count, name)
            var engine = S.Engine(bindings, device: device)
            let t0 = ContinuousClock.now
            func at(_ ms: Int) -> ContinuousClock.Instant { t0 + .milliseconds(ms) }

            // (t, order, event): events before the tick at one t, flush last.
            var timeline: [(t: Int, order: Int, event: S.Event?)] = []
            for e in try XCTUnwrap(c["events"] as? [[String: Any]], name) {
                let bit = try id(e["bit"])
                let event: S.Event = e["down"] as? Bool == true ? .press(bit) : .release(bit)
                timeline.append((try XCTUnwrap(e["t"] as? Int), 0, event))
            }
            for t in try XCTUnwrap(c["ticks"] as? [Int], name) { timeline.append((t, 1, nil)) }
            if let t = c["flush"] as? Int { timeline.append((t, 2, nil)) }
            // Stable: equal keys keep the file's order, as the Rust sort does.
            timeline = timeline.enumerated()
                .sorted { ($0.element.t, $0.element.order, $0.offset) < ($1.element.t, $1.element.order, $1.offset) }
                .map(\.element)

            var got: [String] = []
            for step in timeline {
                let outs: [S.Out]
                switch (step.order, step.event) {
                case (0, let event?): outs = engine.step(event, at: at(step.t))
                case (1, _): outs = engine.tick(at(step.t))
                default: outs = engine.flush()
                }
                got += outs.map { "\(step.t) \($0)" }
            }
            var want: [String] = []
            for e in try XCTUnwrap(c["expect"] as? [[String: Any]], name) {
                let out = try XCTUnwrap(e["out"] as? [String: Any], name)
                let o: S.Out
                if let f = out["forward"] as? [Any] {
                    o = .forward(try id(f[0]), try XCTUnwrap(f[1] as? Bool))
                } else if let f = out["fire"] as? [String] {
                    let phases: [String: S.Phase] = ["once": .once, "down": .down, "up": .up, "tap": .tap]
                    o = .fire(S.Action(id: f[0]), try XCTUnwrap(phases[f[1]], "\(name): a phase"))
                } else {
                    o = .lift(try XCTUnwrap(out["lift"] as? [Any], name).map(id))
                }
                want.append("\(try XCTUnwrap(e["t"] as? Int)) \(o)")
            }
            XCTAssertEqual(got, want, name)
        }
    }

    /// The table's bits are the wire's: `GamepadWire` reads them from the C header.
    func testButtonBitsAreTheWiresAndRoundTrip() {
        let wire: [String: UInt32] = [
            "a": GamepadWire.a, "b": GamepadWire.b, "x": GamepadWire.x, "y": GamepadWire.y,
            "lb": GamepadWire.leftShoulder, "rb": GamepadWire.rightShoulder,
            "ls": GamepadWire.leftStickClick, "rs": GamepadWire.rightStickClick,
            "start": GamepadWire.start, "select": GamepadWire.back, "guide": GamepadWire.guide,
            "up": GamepadWire.dpadUp, "down": GamepadWire.dpadDown, "left": GamepadWire.dpadLeft,
            "right": GamepadWire.dpadRight, "touchpad": GamepadWire.touchpadClick,
            "misc1": GamepadWire.misc1, "paddle1": GamepadWire.paddle1,
            "paddle2": GamepadWire.paddle2, "paddle3": GamepadWire.paddle3,
            "paddle4": GamepadWire.paddle4,
        ]
        for (name, bit) in S.buttons {
            XCTAssertEqual(S.buttonBit(name), bit)
            XCTAssertEqual(S.buttonName(bit), name)
            if let w = wire[name] { XCTAssertEqual(w, bit, name) }
        }
        XCTAssertEqual(S.buttonBit("BACK"), S.buttonBit("select"))
    }

    func testABlobRoundTripsThroughTheEditor() {
        for who in [S.Client.desktop, .android, .apple, .mac, .webOS, .web, .tizen, .ps5, .switch] {
            let d = S.defaults(client: who, gameMode: true, legacyGuideGesture: "on")
            XCTAssertEqual(S.parse(d.toJSON()), d, "\(who)")
        }
    }

    func testAHintNamesTheLiveTrigger() {
        let end = S.Action.slot(.endStream)
        let apple = S.defaults(client: .apple)
        XCTAssertEqual(apple.hint(end, device: .pad), "Hold L1 + R1 + Start + Select")
        XCTAssertEqual(apple.hint(end, device: .keyboard), "⌃⌥⇧D")
        XCTAssertEqual(apple.hint(end, device: .keyboard, client: .desktop), "Ctrl+Alt+Shift+D")
        let moved = S.parse(#"{"v":1,"bindings":[{"on":"pad","keys":["start","select"],"hold":true,"do":"end_stream"}]}"#)
        XCTAssertEqual(moved.hint(end, device: .pad), "Hold Start + Select")
        XCTAssertNil(moved.hint(end, device: .keyboard))
    }

    /// A binding row edits its binding; a fixed row edits nothing. An Apple TV lists the remote
    /// and no keys; a Mac lists its fixed ⌘ rows after the bindings.
    func testRowsPointAtTheirBindings() {
        let mac = S.defaults(client: .mac)
        let rows = mac.rows(client: .mac, touchscreen: false)
        XCTAssertEqual(rows.map(\.kind), [.keyboard, .pad])
        XCTAssertEqual(rows[0].rows.compactMap(\.binding).count, mac.bindings.filter { $0.trigger.device == .keyboard }.count)
        XCTAssertTrue(rows[0].rows.contains { $0.keys == "⌘⎋" && $0.binding == nil })
        XCTAssertEqual(rows[1].rows.first?.keys, "Select + A")
        let tv = S.defaults(client: .apple).rows(client: .apple, touchscreen: false)
        XCTAssertEqual(tv.map(\.kind), [.remote, .pad])
        XCTAssertTrue(tv[0].rows.allSatisfy { $0.binding == nil })
        let desktop = S.defaults(client: .desktop).rows(client: .desktop, touchscreen: false)
        XCTAssertTrue(desktop[0].rows.contains { $0.keys == "F11 or Alt+Enter" })
    }

    /// A key combination row shows the player's name for it, or its keys; a host action this
    /// app knows has its words; a dangling id stays an id.
    func testRowsNameKeyCombinationsAndHostActions() {
        let cfg = OverlayConfig.parse(#"""
            {"v":2,"shortcuts":[{"id":"s1","label":"Task Manager","keys":["ctrl","shift","escape"]},
            {"id":"s2","keys":["alt","tab"]}]}
            """#, platform: .desktop)
        let s = S.parse(#"""
            {"v":1,"bindings":[
            {"on":"pad","keys":["select"],"hold":true,"do":"shortcut:s1"},
            {"on":"pad","keys":["start"],"hold":true,"do":"shortcut:s2"},
            {"on":"pad","keys":["lb","rb"],"do":"shortcut:gone"},
            {"on":"pad","keys":["ls","rs"],"do":"host:display.next"},
            {"on":"pad","keys":["up","down"],"do":"host:custom.eject"}]}
            """#)
        let texts = { (o: OverlayConfig?) in s.rows(client: .apple, touchscreen: false, overlay: o).last?.rows.map(\.text) }
        XCTAssertEqual(texts(cfg), ["Task Manager", "Alt+Tab", "shortcut:gone",
                                    String(localized: L10n.hostactionDisplayNext), "host:custom.eject"])
        XCTAssertEqual(texts(nil)?.first, "shortcut:s1")
    }

    /// The editor checks a changed row against the others; every default row passes its own check.
    func testEveryDefaultRowCanBeEditedInPlace() {
        for who in [S.Client.desktop, .android, .apple, .mac, .webOS, .web, .tizen, .ps5, .switch] {
            let d = S.defaults(client: who, gameMode: true, legacyGuideGesture: "on")
            for (i, b) in d.bindings.enumerated() {
                var others = d
                others.bindings.remove(at: i)
                XCTAssertNil(others.validate(b), "\(who) row \(i): \(b)")
            }
        }
    }
}
