import XCTest

import PunktfunkShared

/// `clients/shared/render-scale-vectors.json` against `RenderScale` — the same cases core and the
/// Kotlin twin run, so every client asks the host for the same `Mode`. Labels stay native.
final class RenderScaleTests: XCTestCase {
    /// Read from the source tree, not copied into the bundle: a copy would be a second contract.
    private static var vectorFileURL: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/render-scale-vectors.json")
    }

    func testEverySharedVectorAgrees() throws {
        let data = try Data(contentsOf: Self.vectorFileURL)
        let root = try XCTUnwrap(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        // JSON has no NaN; the file writes it as null.
        let num = { (v: Any?) in (v as? Double) ?? .nan }
        for row in try XCTUnwrap(root["max_dimension"] as? [[String: Any]]) {
            let codec = try XCTUnwrap(row["codec"] as? String)
            XCTAssertEqual(RenderScale.maxDimension(codec: codec), row["max"] as? Int, codec)
        }
        for row in try XCTUnwrap(root["sanitize"] as? [[String: Any]]) {
            XCTAssertEqual(RenderScale.sanitize(num(row["raw"])), num(row["want"]), "\(row)")
        }
        for row in try XCTUnwrap(root["percent"] as? [[String: Any]]) {
            let typed = try XCTUnwrap(row["typed"] as? Int, "\(row)")
            let want = num(row["want"])
            XCTAssertEqual(RenderScale.fromPercent(typed), want, "\(row)")
            XCTAssertEqual(RenderScale.percent(want), row["reads"] as? Int, "\(row)")
        }
        for row in try XCTUnwrap(root["preset"] as? [[String: Any]]) {
            XCTAssertEqual(
                RenderScale.presetIndex(num(row["scale"])), row["index"] as? Int, "\(row)")
        }
        let cases = try XCTUnwrap(root["apply"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 12, "the vector file is the contract; keep it rich")
        for c in cases {
            let name = c["name"] as? String ?? "?"
            let base = try XCTUnwrap(c["base"] as? [Int], name)
            let want = try XCTUnwrap(c["want"] as? [Int], name)
            let codec = try XCTUnwrap(c["codec"] as? String, name)
            let m = RenderScale.apply(
                baseWidth: base[0],
                baseHeight: base[1],
                scale: num(c["scale"]),
                maxDimension: RenderScale.maxDimension(codec: codec)
            )
            XCTAssertEqual([Int(m.width), Int(m.height)], want, name)
        }
        let views = try XCTUnwrap(root["view"] as? [[String: Any]])
        XCTAssertFalse(views.isEmpty)
        for row in views {
            let base = try XCTUnwrap(row["base"] as? [Int], "\(row)")
            let v = RenderScale.view(
                baseWidth: base[0], baseHeight: base[1], scale: num(row["scale"]),
                maxDimension: try XCTUnwrap(row["max"] as? Int, "\(row)"))
            XCTAssertEqual(v.map { [Int($0.width), Int($0.height)] }, row["want"] as? [Int], "\(row)")
        }
    }

    /// Above 1x a further screen asks for its panel × the scale, shown at its panel; at 1x and
    /// below, and against the codec cap, its panel and no view.
    func testAFurtherScreenSupersamplesLikeTheFirst() {
        let ask = { (w: Int, h: Int, scale: Double, cap: Int) -> [UInt32] in
            let a = RenderScale.screenAsk(baseWidth: w, baseHeight: h, scale: scale, maxDimension: cap)
            return [a.mode.width, a.mode.height] + (a.view.map { [$0.width, $0.height] } ?? [])
        }
        XCTAssertEqual(ask(2560, 1440, 1.5, 8192), [3840, 2160, 2560, 1440])
        XCTAssertEqual(ask(2560, 1440, 1.0, 8192), [2560, 1440])
        XCTAssertEqual(ask(2560, 1440, 0.5, 8192), [2560, 1440])
        XCTAssertEqual(ask(4096, 2304, 2.0, 4096), [4096, 2304])
    }

    func testLabels() {
        XCTAssertEqual(RenderScale.label(1.0), String(localized: L10n.playerRenderScaleNative))
        XCTAssertEqual(
            RenderScale.label(2.0), L10n.playerRenderScaleSupersample(scale: "2"))
        XCTAssertEqual(RenderScale.label(0.5), "0.5×")
    }
}
