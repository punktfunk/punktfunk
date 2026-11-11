// The Shortcuts page's Save: whether an edit stores, asks to replace, or stays off, and the
// bindings it leaves.

import XCTest
@testable import PunktfunkShared

final class ShortcutSaveTests: XCTestCase {
    private typealias S = Shortcuts
    private let selectA = S.Trigger.pad(buttons: [0x0020, 0x1000], hold: false)
    private let selectX = S.Trigger.pad(buttons: [0x0020, 0x4000], hold: false)
    private let selectXY = S.Trigger.pad(buttons: [0x0020, 0x4000, 0x8000], hold: false)
    private let chordQ = S.Trigger.key(modifiers: ["ctrl", "alt", "shift"], key: "q")

    /// Select + A opens the dial, Select + X cycles stats, Ctrl+Alt+Shift+Q releases input.
    private var three: S {
        S(bindings: [
            .init(trigger: selectA, action: .ring),
            .init(trigger: selectX, action: .slot(.stats)),
            .init(trigger: chordQ, action: .slot(.releaseInput)),
        ])
    }

    func testANewTriggerNobodyUsesSaves() {
        let y = S.Trigger.pad(buttons: [0x0020, 0x8000], hold: false)
        XCTAssertEqual(three.saveDecision(y, action: .slot(.mic), editing: nil), .ok)
    }

    func testAnIdenticalTriggerAsksToReplaceIt() {
        XCTAssertEqual(three.saveDecision(selectX, action: .slot(.mic), editing: nil), .replace(1))
        XCTAssertEqual(three.saveDecision(chordQ, action: .slot(.mic), editing: 0), .replace(1),
                       "an index of the others: the edited row is left out")
    }

    func testAnEditKeepingItsOwnTriggerSaves() {
        XCTAssertEqual(three.saveDecision(selectX, action: .slot(.mic), editing: 1), .ok)
    }

    func testAPressInsideAnotherChordAndTheValidatorsRulesBlock() {
        XCTAssertEqual(three.saveDecision(selectXY, action: .slot(.mic), editing: nil),
                       .blocked(.pressSubset(1)))
        XCTAssertEqual(three.saveDecision(.pad(buttons: [0x1000], hold: false), action: .ring, editing: nil),
                       .blocked(.singlePress))
        XCTAssertEqual(three.saveDecision(.key(modifiers: [], key: "q"), action: .ring, editing: nil),
                       .blocked(.noModifier))
        XCTAssertEqual(three.saveDecision(.key(modifiers: ["ctrl"], key: "alt"), action: .ring, editing: nil),
                       .blocked(.noKey))
    }

    func testRemovingLeavesTheOthersInOrder() {
        XCTAssertEqual(three.without(1).bindings.map(\.action), [.ring, .slot(.releaseInput)])
        XCTAssertEqual(three.without(nil), three)
    }

    func testANewBindingAppendsAndAnEditStaysInPlace() {
        let mic = S.Binding(trigger: .pad(buttons: [0x0020, 0x8000], hold: false), action: .slot(.mic))
        XCTAssertEqual(three.applying(binding: mic, at: nil, replacing: nil).bindings.last, mic)
        let edited = three.applying(binding: mic, at: 1, replacing: nil)
        XCTAssertEqual(edited.bindings.map(\.action), [.ring, .slot(.mic), .slot(.releaseInput)])
    }

    /// `replacing` indexes the others: past the edited row it is one further along.
    func testAReplacedBindingGoesWhereverItSits() {
        let mic = S.Binding(trigger: selectX, action: .slot(.mic))
        XCTAssertEqual(three.applying(binding: mic, at: nil, replacing: 1).bindings.map(\.action),
                       [.ring, .slot(.releaseInput), .slot(.mic)])
        let before = S.Binding(trigger: selectA, action: .slot(.mic))
        XCTAssertEqual(three.applying(binding: before, at: 2, replacing: 0).bindings.map(\.action),
                       [.slot(.stats), .slot(.mic)], "the replaced row sits before the edited one")
        let after = S.Binding(trigger: chordQ, action: .slot(.mic))
        XCTAssertEqual(three.applying(binding: after, at: 0, replacing: 1).bindings.map(\.action),
                       [.slot(.mic), .slot(.stats)], "the replaced row sits after the edited one")
    }
}
