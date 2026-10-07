// The Swift twin of pf-client-core's pad-type cycle test: the same order, the same fallback.

import XCTest
@testable import PunktfunkKit

final class PadTypeTests: XCTestCase {
    func testTheCycleWrapsAndASettingsOnlyTypeStepsToAutomatic() {
        var seen: [PunktfunkConnection.GamepadType] = [.auto]
        var p = PunktfunkConnection.GamepadType.auto.nextInRing
        while p != .auto {
            seen.append(p)
            p = p.nextInRing
        }
        XCTAssertEqual(seen, PunktfunkConnection.GamepadType.ringCycle)
        XCTAssertEqual(PunktfunkConnection.GamepadType.steamController2.nextInRing, .auto)
        XCTAssertEqual(PunktfunkConnection.GamepadType.dualShock4.ringLabel, "DualShock 4")
    }

    /// A name with no imageset draws nothing at runtime, so check the catalog itself.
    func testEveryPickedTypeShipsAMark() {
        let catalog = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Sources/PunktfunkKit/Resources/PadMarks.xcassets")
        for type in PunktfunkConnection.GamepadType.ringCycle where type != .auto {
            guard let name = type.markName else { return XCTFail("\(type) has no outline") }
            let pdf = catalog.appendingPathComponent("\(name).imageset/\(name).pdf")
            XCTAssertTrue(FileManager.default.fileExists(atPath: pdf.path), "\(name).pdf missing")
        }
        XCTAssertNil(PunktfunkConnection.GamepadType.auto.mark)
    }
}
