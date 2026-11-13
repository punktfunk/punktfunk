import XCTest

import PunktfunkShared

/// `clients/shared/game-override-vectors.json` against `GameOverrides.fit`: the rows core and the
/// Kotlin twin run, so every client asks the host for the same mode under a game's overrides.
final class GameOverrideVectorsTests: XCTestCase {
    /// Read from the source tree, not copied into the bundle: a copy would be a second contract.
    private static var vectorFileURL: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/game-override-vectors.json")
    }

    func testEveryFitVectorAgrees() throws {
        let data = try Data(contentsOf: Self.vectorFileURL)
        let root = try XCTUnwrap(try JSONSerialization.jsonObject(with: data) as? [String: Any])
        let rows = try XCTUnwrap(root["fit"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(rows.count, 12, "the vector file is the contract")
        for row in rows {
            let why = row["why"] as? String ?? "?"
            let mode = try XCTUnwrap(row["mode"] as? [Int], why)
            let native = try XCTUnwrap(row["native"] as? [Int], why)
            let want = try XCTUnwrap(row["expect"] as? [Int], why)
            let o = GameOverrides(
                lines: (row["lines"] as? Int).map(UInt32.init),
                hz: (row["hz"] as? Int).map(UInt32.init),
                beyondScreen: row["beyond_screen"] as? Bool ?? false)
            let got = o.fit(
                (UInt32(mode[0]), UInt32(mode[1]), UInt32(mode[2])),
                native: (native[0], native[1], native[2]),
                codecMax: try XCTUnwrap(row["max"] as? Int, why))
            XCTAssertEqual([Int(got.width), Int(got.height), Int(got.hz)], want, why)
        }
    }

    /// `lines` comes off the wire as any `UInt32`; the sizes saturate at the largest even value
    /// instead of trapping on the doubling in `even`.
    func testAHugeLinesValueSaturatesInsteadOfTrapping() {
        let o = GameOverrides(lines: .max, beyondScreen: true)
        let got = o.fit(
            (2752, 2064, 120), native: (2752, 2064, 120), codecMax: Int(UInt32.max))
        XCTAssertEqual(got.height, UInt32.max - 1)
        XCTAssertEqual(got.width, UInt32.max - 1)
        XCTAssertEqual(got.hz, 120)
    }

    func testEmptyMeansNothingSet() {
        XCTAssertTrue(GameOverrides().isEmpty)
        XCTAssertTrue(GameOverrides(name: "4K").isEmpty, "a name alone sets nothing")
        XCTAssertFalse(GameOverrides(beyondScreen: true).isEmpty)
    }
}
