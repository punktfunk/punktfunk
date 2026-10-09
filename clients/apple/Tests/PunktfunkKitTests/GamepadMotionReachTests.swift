// Replays `clients/shared/gamepad-kind-vectors.json` against the Swift `GamepadType`: wire value,
// names, motion plane and the per-pad motion answer. Core's `config::tests::gamepad_kind_vectors`
// reads the same file, so a kind core adds and Swift lacks fails here.

import Foundation
import PunktfunkCore
import XCTest

@testable import PunktfunkKit

final class GamepadMotionReachTests: XCTestCase {
    private typealias Pad = PunktfunkConnection.GamepadType
    private typealias Obj = [String: Any]

    /// Read from the source tree, never copied into the bundle: a copy drifts.
    private static func vectors() throws -> Obj {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/gamepad-kind-vectors.json")
        let raw = try JSONSerialization.jsonObject(with: Data(contentsOf: url))
        return try XCTUnwrap(raw as? Obj)
    }

    private static func rows(_ key: String) throws -> [Obj] {
        try XCTUnwrap(try vectors()[key] as? [Obj], "\(key) section")
    }

    /// Swift raw values must be literals, so this is the guard that they are the header's.
    func testRawValuesAreTheHeaderConstants() {
        let header: [(Pad, Int32)] = [
            (.auto, PUNKTFUNK_GAMEPAD_AUTO), (.xbox360, PUNKTFUNK_GAMEPAD_XBOX360),
            (.dualSense, PUNKTFUNK_GAMEPAD_DUALSENSE), (.xboxOne, PUNKTFUNK_GAMEPAD_XBOXONE),
            (.dualShock4, PUNKTFUNK_GAMEPAD_DUALSHOCK4),
            (.steamController, PUNKTFUNK_GAMEPAD_STEAMCONTROLLER),
            (.steamDeck, PUNKTFUNK_GAMEPAD_STEAMDECK),
            (.dualSenseEdge, PUNKTFUNK_GAMEPAD_DUALSENSEEDGE),
            (.switchPro, PUNKTFUNK_GAMEPAD_SWITCHPRO),
            (.steamController2, PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2),
            (.steamController2Puck, PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2_PUCK),
            (.xboxElite, PUNKTFUNK_GAMEPAD_XBOXELITE),
            (.eightBitDoUltimate2, PUNKTFUNK_GAMEPAD_8BITDO_ULTIMATE2),
            (.eightBitDoPro2, PUNKTFUNK_GAMEPAD_8BITDO_PRO2),
            (.eightBitDoPro3, PUNKTFUNK_GAMEPAD_8BITDO_PRO3),
            (.horipadSteam, PUNKTFUNK_GAMEPAD_HORIPAD_STEAM),
            (.joyConPair, PUNKTFUNK_GAMEPAD_JOYCON_PAIR),
            (.switch2Pro, PUNKTFUNK_GAMEPAD_SWITCH2_PRO),
            (.switch2GameCube, PUNKTFUNK_GAMEPAD_SWITCH2_GAMECUBE),
        ]
        for (pad, value) in header {
            XCTAssertEqual(pad.rawValue, UInt32(value), "\(pad)")
        }
        XCTAssertEqual(
            Pad.allCases.map(\.rawValue), Array(0...UInt32(PUNKTFUNK_GAMEPAD_SWITCH2_GAMECUBE)))
    }

    func testEveryKindAnswersWhatCoreAnswers() throws {
        let kinds = try Self.rows("kinds")
        XCTAssertEqual(kinds.count, Pad.allCases.count, "one row per kind, no more, no fewer")
        for row in kinds {
            let value = try XCTUnwrap((row["value"] as? NSNumber)?.uint32Value)
            let pad = try XCTUnwrap(Pad(rawValue: value), "kind \(value) has no Swift case")
            let name = try XCTUnwrap(row["name"] as? String)
            XCTAssertEqual(pad.canonicalName, name)
            XCTAssertEqual(pad.hasMotion, row["has_motion"] as? Bool, name)
            for alias in [name] + (row["aliases"] as? [String] ?? []) {
                XCTAssertEqual(Pad(name: alias), pad, alias)
                XCTAssertEqual(Pad(name: " \(alias.uppercased()) "), pad, alias)
            }
        }
        for name in try XCTUnwrap(try Self.vectors()["rejected_names"] as? [String]) {
            XCTAssertNil(Pad(name: name), name)
        }
    }

    /// The per-pad question, row by row; each `why` says which of the three inputs decides it.
    func testMotionReachIsAnsweredPerPadNotPerSession() throws {
        let rows = try Self.rows("motion_reaches")
        XCTAssertGreaterThan(rows.count, 8, "the vector file is the contract; keep it rich")
        for row in rows {
            let why = row["why"] as? String ?? "?"
            func pad(_ key: String) throws -> Pad {
                try XCTUnwrap(Pad(name: row[key] as? String ?? ""), "\(why): \(key)")
            }
            XCTAssertEqual(
                Pad.motionReaches(
                    declared: try pad("declared"), asked: try pad("asked"),
                    resolved: try pad("resolved")),
                row["want"] as? Bool, why)
        }
    }

    /// The console's compositor names are the host's `CompositorPref` names.
    func testCompositorNamesRoundTrip() {
        typealias Comp = PunktfunkConnection.Compositor
        for comp in Comp.allCases {
            XCTAssertEqual(Comp(name: comp.canonicalName), comp)
        }
        XCTAssertEqual(Comp(name: "plasma"), .kwin)
        XCTAssertEqual(Comp(name: "wlr"), .wlroots)
        XCTAssertEqual(Comp(name: " detect "), .auto)
    }
}
