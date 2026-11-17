// `clients/shared/video-fit-vectors.json` against `VideoFit.place` — the same cases the Rust and
// Kotlin placements run, so a letterbox here cannot land a pixel off another client's.

import CoreGraphics
import XCTest

import PunktfunkShared

final class VideoFitTests: XCTestCase {
    /// Read from the source tree, not copied into the bundle: a copy would be a second contract.
    private static var vectorFileURL: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/video-fit-vectors.json")
    }

    func testEverySharedVectorAgrees() throws {
        let data = try Data(contentsOf: Self.vectorFileURL)
        let root = try XCTUnwrap(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        let cases = try XCTUnwrap(root["cases"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 12, "the vector file is the contract; keep it rich")
        for c in cases {
            let name = c["name"] as? String ?? "?"
            let view = try XCTUnwrap(c["view"] as? [Int], name)
            let frame = try XCTUnwrap(c["frame"] as? [Int], name)
            let p = VideoFit(name: c["fit"] as? String)
                .place(view: (view[0], view[1]), frame: (frame[0], frame[1]))
            let want = try XCTUnwrap(c["expect"] as? [String: Any], name)
            XCTAssertEqual(want["dst"] as? [Int], [p.dstX, p.dstY, p.dstWidth, p.dstHeight], "\(name) dst")
            let src = try XCTUnwrap(want["src"] as? [Double], name)
            for (i, got) in [p.srcX, p.srcY, p.srcWidth, p.srcHeight].enumerated() {
                XCTAssertEqual(got, src[i], accuracy: 1e-4, "\(name) src[\(i)]")
            }
            XCTAssertEqual(want["kernel"] as? [String], [p.kernelX.rawValue, p.kernelY.rawValue], "\(name) kernel")
            for row in want["to_frame"] as? [[Double]] ?? [] {
                let f = p.frame(fromView: CGPoint(x: row[0], y: row[1]))
                XCTAssertEqual(Double(f.x), row[2], accuracy: 1e-4, "\(name) to_frame x")
                XCTAssertEqual(Double(f.y), row[3], accuracy: 1e-4, "\(name) to_frame y")
            }
        }
    }

    func testUnknownNamesReadAsFit() {
        XCTAssertEqual(VideoFit(name: "zoom"), .fit)
        XCTAssertEqual(VideoFit(name: nil), .fit)
        for fit in VideoFit.allCases { XCTAssertEqual(VideoFit(name: fit.rawValue), fit) }
    }

    func testViewMappingRoundTripsUnderCrop() {
        let p = VideoFit.crop.place(view: (3216, 1440), frame: (1920, 1080))
        let f = p.frame(fromView: CGPoint(x: 1608, y: 720))
        let v = p.view(fromFrame: f)
        XCTAssertEqual(Double(v.x), 1608, accuracy: 1e-6)
        XCTAssertEqual(Double(v.y), 720, accuracy: 1e-6)
    }
}
