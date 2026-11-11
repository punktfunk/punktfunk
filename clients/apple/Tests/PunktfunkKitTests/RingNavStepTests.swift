// RingNav.step and .fire: the ring's pad navigation and the arm-then-confirm rule, a row each.

import XCTest

@testable import PunktfunkKit

final class RingNavStepTests: XCTestCase {
    private typealias S = RingNav.State
    private typealias Row = RingNav.Row

    /// End stream in slot 0, a dimmed mic in 1, Stats in 2, slot 3 empty, a host reboot in 4.
    private let ring: [Row?] = [
        Row(id: "end_stream", twoPress: true), Row(id: "mic", enabled: false), Row(id: "stats"),
        nil, Row(id: "host:power.reboot", twoPress: true), Row(id: "keyboard"),
    ]
    /// End stream, Resolution, a dimmed touch mode.
    private let sheet: [Row?] = [Row(), Row(adjustable: true), Row(enabled: false)]

    private func step(_ s: S, _ e: RingNav, _ rows: [Row?]? = nil) -> (state: S, effects: [RingNav.Effect]) {
        RingNav.step(state: s, event: e, rows: rows ?? (s.sheet ? sheet : ring))
    }

    /// The D-pad steps disc by disc and wraps; from the centre Right lands on 12 o'clock, Left
    /// on 10. Up and Down jump to 12 and 6, Y back to the centre.
    func testTheDpadStepsTheHighlight() {
        let cases: [(Int?, RingNav, Int)] = [
            (nil, .right, 0), (6, .right, 0), (0, .right, 1), (5, .right, 0),
            (nil, .left, 5), (6, .left, 5), (0, .left, 5), (3, .left, 2),
            (4, .up, 0), (1, .down, 3), (2, .centre, 6), (nil, .centre, 6),
        ]
        for (from, e, to) in cases {
            let r = step(S(highlight: from), e)
            XCTAssertEqual(r.state.highlight, to, "\(String(describing: from)) \(e)")
            XCTAssertEqual(r.effects, [.haptic(.press)])
        }
    }

    /// The stick aims: its sector is the highlight and neutral the centre; a sector it already
    /// holds does nothing.
    func testTheStickAimsTheHighlight() {
        XCTAssertEqual(step(S(), .sector(2)).state.highlight, 2)
        XCTAssertEqual(step(S(), .sector(2)).effects, [.haptic(.press)])
        XCTAssertEqual(step(S(highlight: 2), .sector(nil)).state.highlight, 6)
        XCTAssertEqual(step(S(), .sector(nil)).state.highlight, 6)
        XCTAssertEqual(step(S(highlight: 2), .sector(2)).effects, [])
        XCTAssertEqual(step(S(highlight: 6), .sector(nil)).effects, [])
    }

    /// A at the centre opens the sheet on its first row; B closes the ring and forgets the
    /// highlight and the armed slot.
    func testTheCentreOpensTheSheetAndBackCloses() {
        let open = step(S(highlight: 6, cursor: 4), .confirm)
        XCTAssertEqual(open.state, S(highlight: 6, sheet: true, cursor: 0))
        XCTAssertEqual(open.effects, [.haptic(.press)])
        XCTAssertEqual(step(S(), .confirm).state.sheet, true)
        let back = step(S(highlight: 2, cursor: 3, armed: "end_stream"), .back)
        XCTAssertEqual(back.state, S(cursor: 3))
        XCTAssertEqual(back.effects, [.close])
    }

    /// A on an empty slot refuses; on a slot it fires through the arm-then-confirm rule.
    func testConfirmFiresTheHighlight() {
        XCTAssertEqual(step(S(highlight: 3), .confirm).effects, [.haptic(.refuse)])
        XCTAssertEqual(step(S(highlight: 2), .confirm).effects, [.haptic(.press), .fire(2)])
        XCTAssertEqual(step(S(highlight: 0), .confirm).effects, [.haptic(.warn), .ask(0)])
    }

    /// A dimmed slot refuses, says why and disarms; a destructive one arms on its first press,
    /// fires on the second, and a press on another destructive slot arms that one instead.
    func testTheArmThenConfirmRule() {
        let refused = RingNav.fire(state: S(armed: "end_stream"), slot: 1, rows: ring)
        XCTAssertNil(refused.state.armed)
        XCTAssertEqual(refused.effects, [.haptic(.refuse), .explain(1)])

        let armed = RingNav.fire(state: S(), slot: 0, rows: ring)
        XCTAssertEqual(armed.state.armed, "end_stream")
        XCTAssertEqual(armed.effects, [.haptic(.warn), .ask(0)])

        let fired = RingNav.fire(state: armed.state, slot: 0, rows: ring)
        XCTAssertNil(fired.state.armed)
        XCTAssertEqual(fired.effects, [.haptic(.press), .fire(0)])

        let other = RingNav.fire(state: armed.state, slot: 4, rows: ring)
        XCTAssertEqual(other.state.armed, "host:power.reboot")
        XCTAssertEqual(other.effects, [.haptic(.warn), .ask(4)])

        let plain = RingNav.fire(state: armed.state, slot: 2, rows: ring)
        XCTAssertNil(plain.state.armed)
        XCTAssertEqual(plain.effects, [.haptic(.press), .fire(2)])

        XCTAssertEqual(RingNav.fire(state: S(), slot: 3, rows: ring).effects, [])
    }

    /// Up and Down walk the sheet's rows and stop at either end.
    func testTheSheetWalksItsRows() {
        let open = S(sheet: true, cursor: 1)
        XCTAssertEqual(step(open, .up).state.cursor, 0)
        XCTAssertEqual(step(S(sheet: true), .up).state.cursor, 0)
        XCTAssertEqual(step(open, .down).state.cursor, 2)
        XCTAssertEqual(step(S(sheet: true, cursor: 2), .down).state.cursor, 2)
        XCTAssertEqual(step(S(sheet: true), .down, []).state.cursor, 0)
        XCTAssertEqual(step(open, .down).effects, [.haptic(.press)])
    }

    /// Left and Right adjust a row that has values and refuse on any other; A taps the row,
    /// with a refusal on a dimmed one; B leaves the sheet; Y does nothing there.
    func testTheSheetAdjustsAndTapsItsRows() {
        XCTAssertEqual(step(S(sheet: true, cursor: 1), .left).effects, [.adjust(-1), .haptic(.press)])
        XCTAssertEqual(step(S(sheet: true, cursor: 1), .right).effects, [.adjust(1), .haptic(.press)])
        XCTAssertEqual(step(S(sheet: true, cursor: 0), .right).effects, [.haptic(.refuse)])
        XCTAssertEqual(step(S(sheet: true, cursor: 0), .confirm).effects, [.haptic(.press), .tap])
        XCTAssertEqual(step(S(sheet: true, cursor: 2), .confirm).effects, [.haptic(.refuse), .tap])
        XCTAssertEqual(step(S(sheet: true, cursor: 9), .confirm).effects, [])
        let back = step(S(highlight: 6, sheet: true, cursor: 2), .back)
        XCTAssertEqual(back.state, S(highlight: 6, cursor: 2))
        XCTAssertEqual(back.effects, [.haptic(.press)])
        XCTAssertEqual(step(S(sheet: true, cursor: 1), .centre).effects, [])
    }

    /// In the sheet the stick's sectors fold onto the four directions; neutral does nothing.
    func testTheSheetFoldsTheSectors() {
        let on = S(sheet: true, cursor: 1)
        XCTAssertEqual(step(on, .sector(0)).state.cursor, 0)
        XCTAssertEqual(step(on, .sector(3)).state.cursor, 2)
        XCTAssertEqual(step(on, .sector(1)).effects, [.adjust(1), .haptic(.press)])
        XCTAssertEqual(step(on, .sector(2)).effects, [.adjust(1), .haptic(.press)])
        XCTAssertEqual(step(on, .sector(4)).effects, [.adjust(-1), .haptic(.press)])
        XCTAssertEqual(step(on, .sector(5)).effects, [.adjust(-1), .haptic(.press)])
        XCTAssertEqual(step(on, .sector(nil)).effects, [])
    }
}
