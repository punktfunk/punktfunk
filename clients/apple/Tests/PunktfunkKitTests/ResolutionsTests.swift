import XCTest

import PunktfunkShared

/// Pins the aspect-family table (`Resolutions`) the pickers share with the Rust and Kotlin
/// twins: every listed size finds its own family, families are told apart by shape, and a family
/// switch lands on the size nearest the current height.
final class ResolutionsTests: XCTestCase {
    func testEveryListedSizeMapsBackToItsFamily() {
        for (i, a) in Resolutions.aspects.enumerated() {
            for s in a.sizes {
                XCTAssertEqual(Resolutions.aspectOf(s.w, s.h), i, "\(s.w)x\(s.h) → \(a.label)")
                XCTAssertTrue(s.w % 2 == 0 && s.h % 2 == 0, "\(s.w)x\(s.h) has an odd side")
            }
            XCTAssertEqual(a.sizes.map(\.h), a.sizes.map(\.h).sorted(), "\(a.label) ascending")
        }
    }

    func testShapeNotMembership() {
        XCTAssertEqual(Resolutions.aspectOf(1500, 1000), 4, "a custom 3:2")
        XCTAssertEqual(Resolutions.aspectOf(3456, 2234), 1, "a MacBook panel reads 16:10")
        XCTAssertNil(Resolutions.aspectOf(2556, 1179), "a phone panel is nobody's")
        XCTAssertNil(Resolutions.aspectOf(0, 0), "native")
    }

    /// A OnePlus-shaped phone: 3216×1440, 127 px of cutout on one side. Twin of the core test.
    func testAPhoneLeadsWithItsScreenAndSafeArea() {
        let f = Resolutions.families(screen: (3216, 1440), safe: (3088, 1440))
        XCTAssertEqual(f.prefix(3).map(\.id), [Resolutions.screenID, Resolutions.safeAreaID, "16:9"])
        XCTAssertEqual(
            f.prefix(3).map(\.label),
            [
                String(localized: L10n.playerResolutionAspectScreen),
                String(localized: L10n.playerResolutionAspectSafeArea), "16:9",
            ])
        XCTAssertEqual(f[0].sizes.map { "\($0.w)x\($0.h)" }, ["1608x720", "2412x1080", "3216x1440"])
        XCTAssertEqual(f[1].sizes.map { "\($0.w)x\($0.h)" }, ["1544x720", "2316x1080", "3088x1440"])
        XCTAssertEqual(Resolutions.familyOf(f, 2412, 1080), 0)
        XCTAssertEqual(Resolutions.familyOf(f, 2316, 1080), 1)
        XCTAssertEqual(Resolutions.familyOf(f, 1920, 1080), 2)
        XCTAssertEqual(
            Resolutions.families(screen: (1920, 1080), safe: (1920, 1080)).count,
            Resolutions.aspects.count, "a standard screen adds nothing")
    }

    /// `clients/shared/custom-resolution-vectors.json`, read from the source tree like the
    /// render-scale vectors: every client clamps a typed size the same way.
    func testCustomMatchesTheSharedVectors() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/custom-resolution-vectors.json")
        let root = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let cases = try XCTUnwrap(root["custom"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 10, "the vector file is the contract; keep it rich")
        for c in cases {
            let name = c["name"] as? String ?? "?"
            let typed = try XCTUnwrap(c["typed"] as? [Int], name)
            let want = try XCTUnwrap(c["want"] as? [Int], name)
            let got = Resolutions.custom(
                typed[0], typed[1], codec: try XCTUnwrap(c["codec"] as? String, name))
            XCTAssertEqual([got.w, got.h], want, name)
        }
    }

    func testNearestFollowsHeightAndNativeMeans1080() {
        XCTAssertTrue(Resolutions.nearest(0, height: 0) == (1920, 1080))
        XCTAssertTrue(Resolutions.nearest(1, height: 1080) == (1920, 1200))
        XCTAssertTrue(Resolutions.nearest(2, height: 1440) == (3440, 1440))
        XCTAssertTrue(Resolutions.nearest(3, height: 2160) == (7680, 2160))
        XCTAssertTrue(Resolutions.nearest(4, height: 800) == (2160, 1440))
        XCTAssertTrue(Resolutions.nearest(5, height: 1000) == (1600, 1200))
    }
}
