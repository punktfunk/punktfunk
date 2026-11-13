// `Sc2Capture.refusedWrite` against `clients/shared/pad-write-vectors.json`, which the desktop
// core's `refused` and Kotlin's `hostWriteRefused` replay too.

import XCTest

@testable import PunktfunkKit

final class PadWriteVectorTests: XCTestCase {
    func testHostWritesMatchTheSharedVectors() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/pad-write-vectors.json")
        let root = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let bytes = { (key: Any?) throws -> [UInt8] in
            try XCTUnwrap(key as? [Int]).map { UInt8($0) }
        }
        let forwarded = try bytes(root["classic_forwarded"])
        let heldBack = try bytes(root["triton_held_back"])
        for cmd in UInt8.min...UInt8.max {
            let classic = Sc2Capture.refusedWrite(classic: true, kind: 1, data: [0, cmd, 0])
            XCTAssertEqual(classic != nil, !forwarded.contains(cmd), "2015 pad 0x\(String(cmd, radix: 16))")
            let triton = Sc2Capture.refusedWrite(classic: false, kind: 1, data: [1, cmd, 0])
            XCTAssertEqual(triton != nil, heldBack.contains(cmd), "triton 0x\(String(cmd, radix: 16))")
        }
        for row in try XCTUnwrap(root["frames"] as? [[String: Any]]) {
            let model = try XCTUnwrap(row["model"] as? String)
            XCTAssert(model == "classic" || model == "triton", model)
            let kind = try UInt8(XCTUnwrap(row["kind"] as? Int))
            let data = try bytes(row["data"])
            let want = try XCTUnwrap(row["refused"] as? Bool)
            let got = Sc2Capture.refusedWrite(classic: model == "classic", kind: kind, data: data)
            XCTAssertEqual(got != nil, want, "\(row)")
        }
    }
}
