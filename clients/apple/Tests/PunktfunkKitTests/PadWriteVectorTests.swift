// `Sc2Capture.refusedWrite` and the Bluetooth framing against
// `clients/shared/pad-write-vectors.json`, which the desktop core's `refused` and Kotlin's
// `hostWriteRefused` and `Sc2Device` replay too.

import XCTest

@testable import PunktfunkKit

final class PadWriteVectorTests: XCTestCase {
    private func shared() throws -> [String: Any] {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/pad-write-vectors.json")
        return try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
    }

    private func bytes(_ key: Any?) throws -> [UInt8] {
        try XCTUnwrap(key as? [Int]).map { UInt8($0) }
    }

    func testBluetoothWritesMatchTheSharedVectors() throws {
        let root = try shared()
        let heldBack = try bytes(root["triton_held_back"])
        for cmd in UInt8.min...UInt8.max {
            let feature = Sc2Device.featurePayload(frame: [1, cmd, 0])
            XCTAssertEqual(feature == nil, heldBack.contains(cmd), "feature 0x\(String(cmd, radix: 16))")
            let output = Sc2Device.outputWrite(frame: [cmd, 0])
            XCTAssertEqual(output != nil, (0x80...0x89).contains(cmd), "output 0x\(String(cmd, radix: 16))")
            XCTAssertNotEqual(output?.charUUID, Sc2Device.reportCharUUID.lowercased())
        }
        for row in try XCTUnwrap(root["triton_ble"] as? [[String: Any]]) {
            let kind = try XCTUnwrap(row["kind"] as? Int)
            let data = try bytes(row["data"])
            let want = try XCTUnwrap(row["refused"] as? Bool)
            let refused = kind == 0
                ? Sc2Device.outputWrite(frame: data) == nil : Sc2Device.featurePayload(frame: data) == nil
            XCTAssertEqual(refused, want, "\(row)")
        }
    }

    func testHostWritesMatchTheSharedVectors() throws {
        let root = try shared()
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
