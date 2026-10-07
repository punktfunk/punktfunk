// The captured SC2's local-chord + ring state machine (`Sc2RingGate`), driven report by report
// the way the capture drives it — `read`, then `apply`. Nothing here needs a radio or a
// connection, which is the point of the type existing.
//
// What the tests pin: the ring's Select-first ordering, that a chord the client consumed never
// reaches the game, and that a swallowed button's release is swallowed too — the failure of the
// last one is a button stuck down in a game with no way to lift it.

import XCTest

@testable import PunktfunkKit

final class Sc2RingGateTests: XCTestCase {
    private func state(_ buttons: UInt32, lsX: Int32 = 0, lsY: Int32 = 0) -> Sc2Device.State {
        var s = Sc2Device.State()
        s.buttons = buttons
        s.lsX = lsX
        s.lsY = lsY
        return s
    }

    /// One report through the whole path: what the gate reports, and what is left to forward.
    private func step(
        _ gate: Sc2RingGate, _ buttons: UInt32, lsX: Int32 = 0, lsY: Int32 = 0
    ) -> (events: [Sc2RingGate.Event], forwarded: Sc2Device.State) {
        var s = state(buttons, lsX: lsX, lsY: lsY)
        let events = gate.read(s)
        gate.apply(&s)
        return (events, s)
    }

    func testRingOpensOnSelectThenA() {
        let gate = Sc2RingGate()
        XCTAssertEqual(step(gate, Sc2Device.btnView).events, [])
        let opened = step(gate, Sc2Device.btnView | Sc2Device.btnA)
        XCTAssertEqual(opened.events, [.ringChord])
        // Neither button reaches the game — they are the dial's, not the game's.
        XCTAssertEqual(opened.forwarded.buttons, 0)
    }

    func testAThenSelectIsAGameCombo() {
        // A first, Select second: a game's own combo, and GamepadCapture would not open the dial
        // for it either. Both buttons forward untouched.
        let gate = Sc2RingGate()
        XCTAssertEqual(step(gate, Sc2Device.btnA).events, [])
        let both = step(gate, Sc2Device.btnA | Sc2Device.btnView)
        XCTAssertEqual(both.events, [])
        XCTAssertEqual(both.forwarded.buttons, Sc2Device.btnA | Sc2Device.btnView)
    }

    func testSwallowedButtonsStayOutUntilTheyRelease() {
        let gate = Sc2RingGate()
        _ = step(gate, Sc2Device.btnView)
        XCTAssertEqual(step(gate, Sc2Device.btnView | Sc2Device.btnA).events, [.ringChord])
        // Still held a frame later, with a third button pressed on top: only the chord's two are
        // held back, and the newcomer plays.
        let held = step(gate, Sc2Device.btnView | Sc2Device.btnA | Sc2Device.btnB)
        XCTAssertEqual(held.forwarded.buttons, Sc2Device.btnB)
        // A releases; Select is still down and still swallowed.
        let lifted = step(gate, Sc2Device.btnView)
        XCTAssertEqual(lifted.forwarded.buttons, 0)
        // Everything released, then A alone: the swallow set is empty, so it reaches the game.
        _ = step(gate, 0)
        XCTAssertEqual(step(gate, Sc2Device.btnA).forwarded.buttons, Sc2Device.btnA)
    }

    func testStatsChordFiresOnceAndStillForwards() {
        let gate = Sc2RingGate()
        XCTAssertEqual(step(gate, Sc2Device.btnView).events, [])
        let fired = step(gate, Sc2Device.btnView | Sc2Device.btnX)
        XCTAssertEqual(fired.events, [.statsChord])
        // A local overlay change is not input the host must not see (GamepadCapture's rule).
        XCTAssertEqual(fired.forwarded.buttons, Sc2Device.btnView | Sc2Device.btnX)
        // A third button on top finds the mask already complete: one cycle per chord.
        XCTAssertEqual(step(gate, Sc2Device.btnView | Sc2Device.btnX | Sc2Device.btnB).events, [])
    }

    func testOpenRingNavigatesAndSendsNothingToTheGame() {
        let gate = Sc2RingGate()
        gate.ringOpen = true
        let down = step(gate, Sc2Device.btnDpadDown)
        XCTAssertEqual(down.events, [.nav(.down)])
        XCTAssertEqual(down.forwarded.buttons, 0)
        // Edge-triggered: a held direction does not repeat here (the ring owns the repeat).
        XCTAssertEqual(step(gate, Sc2Device.btnDpadDown).events, [])
        XCTAssertEqual(step(gate, Sc2Device.btnDpadDown | Sc2Device.btnA).events, [.nav(.confirm)])

        // The stick aims by sector, and reaches the game as neutral either way. Six 60° sectors
        // clockwise from 12 o'clock puts a hard right on the 2/3 boundary, which rounds to 2.
        let aimed = step(gate, 0, lsX: 32767)
        XCTAssertEqual(aimed.events, [.nav(.sector(2))])
        XCTAssertEqual(aimed.forwarded.lsX, 0)
        XCTAssertEqual(step(gate, 0, lsX: 32767).events, []) // same sector, no repeat
        XCTAssertEqual(step(gate, 0).events, [.nav(.sector(nil))]) // back to neutral

        // Closing hands the pad back, but a press the ring already ate does not fire in the game
        // on the way out: it plays only after a release and a fresh press.
        _ = step(gate, Sc2Device.btnA)
        gate.ringOpen = false
        XCTAssertEqual(step(gate, Sc2Device.btnA).forwarded.buttons, 0)
        _ = step(gate, 0)
        XCTAssertEqual(step(gate, Sc2Device.btnA).forwarded.buttons, Sc2Device.btnA)
    }

    func testHardwareMaskIgnoresTheGate() {
        // The escape chord reads this: no client-side gate may hide the way out of a stream.
        let gate = Sc2RingGate()
        gate.ringOpen = true
        let escape = Sc2Device.sc2Buttons(forWire: Sc2Capture.escapeChord)
        _ = step(gate, escape)
        XCTAssertEqual(gate.hardware & Sc2Capture.escapeChord, Sc2Capture.escapeChord)
        XCTAssertEqual(gate.swallow & escape, escape) // …while the game still sees none of it
    }
}
