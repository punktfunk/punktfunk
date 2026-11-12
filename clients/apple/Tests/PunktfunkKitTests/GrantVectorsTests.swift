// `crates/core/punktfunk-core/testdata/grant-vectors.json`, which core writes, against the Swift
// grant rule: the wire reader's legacy-full read (`normalized`) and the level a mask labels as.
// Kotlin replays the same columns; the web console replays the stored ones.

import XCTest

@testable import PunktfunkKit

final class GrantVectorsTests: XCTestCase {
    func testLevelsMatchTheCoreVectors() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .deletingLastPathComponent() // repo root
            .appendingPathComponent("crates/core/punktfunk-core/testdata/grant-vectors.json")
        let root = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        XCTAssertEqual(root["all"] as? Int, Int(PunktfunkConnection.grantAll))
        XCTAssertEqual(root["all_pre_power"] as? Int, Int(PunktfunkConnection.grantAllPrePower))
        XCTAssertEqual(root["all_pre_manage"] as? Int, Int(PunktfunkConnection.grantAllPreManage))
        XCTAssertEqual(
            root["all_pre_display"] as? Int, Int(PunktfunkConnection.grantAllPreDisplay))
        let bits = try XCTUnwrap(root["bits"] as? [String: Int])
        XCTAssertEqual(bits["DISPLAY"], Int(PunktfunkConnection.grantDisplay))
        let levels: [String: PunktfunkConnection.AccessLevel] = [
            "full": .fullControl, "controller": .controllerOnly, "view": .viewOnly,
            "custom": .custom,
        ]
        for c in try XCTUnwrap(root["masks"] as? [[String: Any]]) {
            let mask = try UInt32(XCTUnwrap(c["mask"] as? Int))
            let normalized = try UInt32(XCTUnwrap(c["normalized"] as? Int))
            XCTAssertEqual(PunktfunkConnection.normalizedGrants(mask), normalized, "mask \(mask)")
            let level = try XCTUnwrap(levels[c["level"] as? String ?? ""], "mask \(mask)")
            XCTAssertEqual(PunktfunkConnection.AccessLevel(grants: mask), level, "mask \(mask)")
        }
    }
}
