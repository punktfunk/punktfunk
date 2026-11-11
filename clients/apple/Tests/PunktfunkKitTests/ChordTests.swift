// `Chord`: the shortcut editors' modifier toggles and key, the Swift twin of pf-client-core's
// `Chord` (its test `a_chord_folds_aliases_and_sends_modifiers_first`).

import XCTest
@testable import PunktfunkShared

final class ChordTests: XCTestCase {
    func testTheKeyboardHoldsEveryKeyOnceAndNoModifier() {
        let keys = Chord.keyGroups.flatMap(\.keys)
        XCTAssertEqual(keys.count, 66, "the Rust KEY_GRID's size")
        XCTAssertEqual(Set(keys).count, keys.count)
        for k in keys {
            XCTAssertNotNil(keyVk(k), k)
            XCTAssertFalse(Chord.modifiers.contains(k), k)
        }
        for m in Chord.modifiers { XCTAssertNotNil(keyVk(m), m) }
    }

    func testModifiersGoOutFirstInSendOrder() {
        let c = Chord.parse(keys: ["ctrl", "alt", "q"])
        XCTAssertEqual(c, Chord(mods: ["ctrl", "alt"], key: "q"))
        XCTAssertEqual(c.keys, ["ctrl", "alt", "q"])
        XCTAssertEqual(Chord.parse(keys: ["shift", "ctrl"]).keys, ["ctrl", "shift"])
    }

    func testAModifierAloneIsACombination() {
        let win = Chord.parse(keys: ["win"])
        XCTAssertEqual(win, Chord(mods: ["win"]))
        XCTAssertEqual(win.keys, ["win"])
        XCTAssertEqual(chordChip(win.keys), "Win")
    }

    func testAChipTogglesItsModifierAndAKeyReplacesTheKey() {
        var c = Chord.parse(keys: ["ctrl", "q"])
        c.toggle("alt")
        XCTAssertEqual(c.keys, ["ctrl", "alt", "q"])
        c.toggle("ctrl")
        XCTAssertEqual(c.keys, ["alt", "q"])
        c.key = "f4"
        XCTAssertEqual(c.keys, ["alt", "f4"])
        XCTAssertEqual(Chord.parse(keys: []).keys, [])
    }
}
