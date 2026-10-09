// Replays `clients/shared/overlay-actions-vectors.json`, which the Rust and Kotlin parsers replay
// too, so the three cannot drift. A new case belongs in that file.

import XCTest
@testable import PunktfunkKit
@testable import PunktfunkShared

final class OverlayActionsTests: XCTestCase {
    /// Read from the repo, not a bundle copy: four levels up from this file is `clients/`.
    private func vectors() throws -> [String: Any] {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<4 { url.deleteLastPathComponent() }
        url.appendPathComponent("shared/overlay-actions-vectors.json")
        let file = try JSONSerialization.jsonObject(with: Data(contentsOf: url))
        return try XCTUnwrap(file as? [String: Any])
    }

    /// JSON equality with every number compared as a Float: each client prints the floats its own way.
    private func sameJSON(_ a: Any, _ b: Any) -> Bool {
        switch (a, b) {
        case let (x as String, y as String): return x == y
        case let (x as NSNumber, y as NSNumber): return x.floatValue == y.floatValue
        case let (x as [Any], y as [Any]):
            return x.count == y.count && zip(x, y).allSatisfy { sameJSON($0, $1) }
        case let (x as [String: Any], y as [String: Any]):
            return Set(x.keys) == Set(y.keys) && x.allSatisfy { k, v in y[k].map { sameJSON(v, $0) } ?? false }
        case (is NSNull, is NSNull): return true
        default: return false
        }
    }

    func testSharedVectorsParseAndRoundTrip() throws {
        let file = try vectors()
        for id in try XCTUnwrap(file["slot_ids"] as? [String]) {
            XCTAssertEqual(SlotId.parse(id)?.id, id)
        }
        let cases = try XCTUnwrap(file["cases"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 10, "the vector file is the contract")
        let platforms: [String: RingPlatform] = ["touch": .touch, "desktop": .desktop]
        for c in cases {
            let name = c["name"] as? String ?? "?"
            let platform = try XCTUnwrap(platforms[c["platform"] as? String ?? ""], name)
            let cfg = OverlayConfig.parse(c["blob"] as? String, platform: platform)
            let want = try XCTUnwrap(c["ring"] as? [Any], name).map { $0 as? String }
            XCTAssertEqual(cfg.ring.map { $0?.id }, want, "\(name): ring")
            let stored = cfg.toJSON()
            let json = try JSONSerialization.jsonObject(with: Data(stored.utf8))
            XCTAssertTrue(sameJSON(json, c["round_trip"] as Any), "\(name): stored \(stored)")
            XCTAssertEqual(OverlayConfig.parse(stored, platform: platform), cfg, "\(name): reparse")
        }
    }

    func testSharedPadTypeCycle() throws {
        typealias Pad = PunktfunkConnection.GamepadType
        let file = try vectors()
        let rows = try XCTUnwrap(file["pad_type_cycle"] as? [[String: String]])
        let cycle = try rows.map { row -> Pad in
            let pad = try XCTUnwrap(Pad(name: row["name"] ?? ""), "\(row)")
            XCTAssertEqual(pad.ringLabel, row["label"])
            return pad
        }
        XCTAssertEqual(cycle, Pad.ringCycle)
        for (i, pad) in cycle.enumerated() {
            XCTAssertEqual(pad.nextInRing, cycle[(i + 1) % cycle.count])
        }
        for name in try XCTUnwrap(file["pad_type_outside_cycle"] as? [String]) {
            XCTAssertEqual(try XCTUnwrap(Pad(name: name)).nextInRing, .auto, name)
        }
    }

    /// Every case Rust's `key_vk` wrote, read from the repo: five levels up from this file is
    /// the root.
    func testKeyVkMatchesTheRustVectors() throws {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { url.deleteLastPathComponent() }
        url.appendPathComponent("crates/punktfunk-core/testdata/key-vk-vectors.json")
        let cases = try JSONDecoder().decode(KeyVkVectors.self, from: Data(contentsOf: url)).cases
        XCTAssertFalse(cases.isEmpty)
        let wrong = cases.filter { keyVk($0.name) != $0.vk }.map {
            "\($0.name.debugDescription): Rust \($0.vk as Any), Swift \(keyVk($0.name) as Any)"
        }
        XCTAssertTrue(wrong.isEmpty, wrong.joined(separator: "\n"))
    }

    func testChordLegends() {
        XCTAssertEqual(chordChip(["ctrl", "shift", "escape"]), "Ctrl+Shift+Esc")
        XCTAssertEqual(keyLegend("win"), "Win")
        XCTAssertEqual(keyLegend("pageup"), "PgUp")
        XCTAssertEqual(keyLegend("f4"), "F4")
        XCTAssertEqual(keyLegend("left"), "←")
    }
}

extension OverlayActionsTests {
    func testTvDefaultRingOffersOnlyWhatATvCanRun() {
        let tv = OverlayConfig.platformDefault(.tv)
        XCTAssertEqual(tv.ring, [.endStream, .disconnectLinger, .stats, .guide, .qam, nil])
        XCTAssertEqual(OverlayConfig.parse("", platform: .tv), tv)
        XCTAssertEqual(OverlayConfig.parse(tv.toJSON(), platform: .tv), tv)
    }

    func testANewShortcutTakesTheFirstEmptySlotAndRemovalEmptiesIt() {
        var cfg = OverlayConfig.platformDefault(.tv)
        cfg.saveShortcut(OverlayShortcut(id: cfg.nextShortcutID, keys: ["ctrl", "escape"]))
        XCTAssertEqual(cfg.ring[5], .shortcut("s1"))
        XCTAssertEqual(cfg.nextShortcutID, "s2")
        cfg.saveShortcut(OverlayShortcut(id: "s1", label: "Menu", keys: ["escape"]))
        XCTAssertEqual(cfg.shortcuts.count, 1, "saving an existing id replaces it")
        XCTAssertEqual(cfg.shortcut("s1")?.label, "Menu")
        cfg.removeShortcut("s1")
        XCTAssertTrue(cfg.shortcuts.isEmpty)
        XCTAssertNil(cfg.ring[5], "the slot that sent it is empty")
    }
}

private struct KeyVkCase: Decodable {
    let name: String
    let vk: UInt32?
}

private struct KeyVkVectors: Decodable {
    let cases: [KeyVkCase]
}
