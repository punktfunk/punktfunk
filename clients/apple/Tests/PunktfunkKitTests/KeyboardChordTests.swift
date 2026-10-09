import Foundation
import XCTest

@testable import PunktfunkKit

/// Pins the ⌃⌥⇧ chords: the iPad's match on the GameController key stream, from HID usages
/// through `hidToVK`, and each platform's table against the shortcuts reference.
final class KeyboardChordTests: XCTestCase {
    private func held(_ usages: [Int]) -> Set<UInt32> {
        Set(usages.compactMap { InputCapture.hidToVK[$0] })
    }

    // HID usages: left Control/Shift/Option 0xE0/0xE1/0xE2, right 0xE4/0xE5/0xE6, Command 0xE3.
    func testEitherSideOfEachModifierCounts() {
        XCTAssertTrue(InputCapture.holdsChordModifiers(held([0xE0, 0xE2, 0xE1])))
        XCTAssertTrue(InputCapture.holdsChordModifiers(held([0xE4, 0xE6, 0xE5])))
        XCTAssertTrue(InputCapture.holdsChordModifiers(held([0xE0, 0xE6, 0xE1])))
    }

    func testAMissingModifierIsNoChord() {
        XCTAssertFalse(InputCapture.holdsChordModifiers([]))
        XCTAssertFalse(InputCapture.holdsChordModifiers(held([0xE0, 0xE2])))
        XCTAssertFalse(InputCapture.holdsChordModifiers(held([0xE0, 0xE4, 0xE1, 0xE5])))
        XCTAssertFalse(InputCapture.holdsChordModifiers(held([0xE3, 0xE2, 0xE1])))
    }

    /// The handler compares letters as VKs: O toggles the ring, Q releases, A mutes, D
    /// disconnects, S cycles the stats.
    func testChordLettersAreTheVKsTheirKeysSend() {
        XCTAssertEqual(InputCapture.hidToVK[0x12], 0x4F) // O
        XCTAssertEqual(InputCapture.hidToVK[0x14], 0x51) // Q
        XCTAssertEqual(InputCapture.hidToVK[0x04], 0x41) // A
        XCTAssertEqual(InputCapture.hidToVK[0x07], 0x44) // D
        XCTAssertEqual(InputCapture.hidToVK[0x16], 0x53) // S
    }

    /// Each platform's chord table holds exactly the ⌃⌥⇧ letters `ShortcutsCatalog` lists for it,
    /// read from source because the app target is not importable. ⌃⌥⇧C is the Stream menu's alone.
    func testChordTablesMatchTheShortcutsReference() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .appendingPathComponent("Sources/PunktfunkClient/Settings/ShortcutsReference.swift")
        let source = try String(contentsOf: url, encoding: .utf8)
        func listed(from start: String, to end: String) throws -> Set<String> {
            let from = try XCTUnwrap(source.range(of: start), start).upperBound
            let to = try XCTUnwrap(source.range(of: end, range: from..<source.endIndex), end)
            return Set(source[from..<to.lowerBound].matches(of: #/⌃⌥⇧([A-Z])/#).map { String($0.1) })
        }
        func letters(_ chords: [InputCapture.Chord]) -> Set<String> {
            Set(chords.map { String(UnicodeScalar(UInt8($0.vk))) })
        }
        let menuOnly: Set<String> = ["C"]
        let mac = try listed(from: "#if os(macOS)", to: "#elseif os(iOS) || os(visionOS)")
        let pad = try listed(from: "#elseif os(iOS) || os(visionOS)", to: "#elseif os(tvOS)")
        XCTAssertEqual(letters(InputCapture.macChords), mac.subtracting(menuOnly))
        XCTAssertEqual(letters(InputCapture.padChords), pad.subtracting(menuOnly))
    }
}
