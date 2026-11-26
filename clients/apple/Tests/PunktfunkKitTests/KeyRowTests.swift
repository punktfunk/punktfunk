// Replays `clients/shared/key-row-vectors.json`, which the Android client replays too: what a
// keyboard row button sends on a tap and a long press, and what the latch does with typed text.
// A new rule belongs in that file.

import XCTest
@testable import PunktfunkKit
@testable import PunktfunkShared

final class KeyRowTests: XCTestCase {
    /// Read from the repo, not a bundle copy: four levels up from this file is `clients/`.
    private func vectors() throws -> [String: Any] {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<4 { url.deleteLastPathComponent() }
        url.appendPathComponent("shared/key-row-vectors.json")
        let file = try JSONSerialization.jsonObject(with: Data(contentsOf: url))
        return try XCTUnwrap(file as? [String: Any])
    }

    private func hex(_ vk: UInt32) -> String { String(format: "%02X", vk) }

    private func word(_ out: KeyRowOut) -> String {
        switch out {
        case .key(let vk, let down): return (down ? "+" : "-") + hex(vk)
        case .text(let text): return "text:\(text)"
        case .fire(let action, let phase): return "fire:\(phase):\(action.id)"
        }
    }

    func testSharedVectorsSendWhatTheySay() throws {
        let cases = try XCTUnwrap(try vectors()["cases"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 10, "the vector file is the contract")
        for c in cases {
            let name = c["name"] as? String ?? "?"
            var row = KeyRowEngine()
            var out: [KeyRowOut] = []
            for step in try XCTUnwrap(c["steps"] as? [[String]], name) {
                let button = step.count > 1 ? RowSlot.parse(step[1]) : nil
                switch step[0] {
                case "tap": out += row.tap(try XCTUnwrap(button, "\(name): \(step)"))
                case "hold": out += row.hold(try XCTUnwrap(button, "\(name): \(step)"))
                case "lift": out += row.lift(try XCTUnwrap(button, "\(name): \(step)"))
                case "type": out += try XCTUnwrap(row.type(step[1]), "\(name): typed with no latch")
                case "release": out += row.release()
                default: XCTFail("\(name): step \(step)")
                }
            }
            XCTAssertEqual(out.map(word), c["out"] as? [String], name)
        }
    }

    func testATypedCharacterFindsItsKeyInTheUsOrder() throws {
        let typed = try XCTUnwrap(try vectors()["typed"] as? [[Any]])
        XCTAssertFalse(typed.isEmpty)
        for row in typed {
            let text = try XCTUnwrap(row[0] as? String)
            XCTAssertEqual(KeyRowEngine.typedVk(try XCTUnwrap(text.first)).map(hex), row[1] as? String, text)
        }
    }

    /// Typing with nothing latched is not the row's: the stream view sends it as it always does.
    func testTypingWithNoLatchIsLeftToTheCaller() {
        var row = KeyRowEngine()
        XCTAssertNil(row.type("c"))
        _ = row.tap(.key("ctrl"))
        XCTAssertTrue(row.isLatched(.key("control")))
        XCTAssertNotNil(row.type("c"))
        XCTAssertFalse(row.isLatched(.key("ctrl")))
    }
}
