// The captured SC2's shortcut + ring state machine (`Sc2RingGate`), driven report by report the
// way the capture drives it — `read`, then `apply`. Nothing here needs a radio or a connection,
// which is the point of the type existing.
//
// What the tests pin: the engine's rules hold on the typed plane, a press a shortcut took never
// reaches the game, and a swallowed button's release is swallowed too — the failure of the last
// one is a button stuck down in a game with no way to lift it.

import XCTest

@testable import PunktfunkKit

final class Sc2RingGateTests: XCTestCase {
    private let t0 = ContinuousClock.now

    private func gate() -> Sc2RingGate {
        Sc2RingGate(Shortcuts.defaults(client: .apple).bindings)
    }

    /// One report at `ms` through the whole path: what the gate reports, and what is left to forward.
    private func step(
        _ gate: Sc2RingGate, _ buttons: UInt32, at ms: Int = 0, lsX: Int32 = 0
    ) -> (events: [Sc2RingGate.Event], forwarded: Sc2Device.State) {
        var s = Sc2Device.State()
        s.buttons = buttons
        s.lsX = lsX
        let events = gate.read(s, at: t0 + .milliseconds(ms))
        gate.apply(&s)
        return (events, s)
    }

    func testRingOpensOnSelectThenAAndNeitherReachesTheGame() {
        let g = gate()
        // Select is held back: Hold Select is the guide by default on Apple.
        XCTAssertEqual(step(g, Sc2Device.btnView).forwarded.buttons, 0)
        let opened = step(g, Sc2Device.btnView | Sc2Device.btnA, at: 80)
        XCTAssertEqual(opened.events, [.fire(.ring, .once)])
        XCTAssertEqual(opened.forwarded.buttons, 0)
        // Still held with B on top: only B plays; then the chord's releases stay off the wire.
        XCTAssertEqual(step(g, Sc2Device.btnView | Sc2Device.btnA | Sc2Device.btnB, at: 100).forwarded.buttons,
                       Sc2Device.btnB)
        XCTAssertEqual(step(g, Sc2Device.btnView, at: 150).forwarded.buttons, 0)
        _ = step(g, 0, at: 200)
        XCTAssertEqual(step(g, Sc2Device.btnA, at: 300).forwarded.buttons, Sc2Device.btnA)
    }

    func testASelectTapGoesOutAtReleaseAndLiftsAfterTapPress() {
        let g = gate()
        _ = step(g, Sc2Device.btnView)
        XCTAssertEqual(step(g, 0, at: 120).forwarded.buttons, Sc2Device.btnView)
        XCTAssertEqual(step(g, 0, at: 169).forwarded.buttons, Sc2Device.btnView)
        XCTAssertEqual(step(g, 0, at: 170).forwarded.buttons, 0)
    }

    func testTheExitHoldEndsTheStreamEvenWithTheRingUp() {
        let g = gate()
        g.ringOpen = true
        let exit = Sc2Device.sc2Buttons(
            forWire: GamepadWire.leftShoulder | GamepadWire.rightShoulder | GamepadWire.start | GamepadWire.back)
        _ = step(g, exit)
        XCTAssertEqual(step(g, exit, at: 999).events, [])
        XCTAssertEqual(step(g, exit, at: 1000).events, [.fire(.slot(.endStream), .once)])
    }

    func testOpenRingNavigatesAndSendsNothingToTheGame() {
        let g = gate()
        g.ringOpen = true
        let down = step(g, Sc2Device.btnDpadDown)
        XCTAssertEqual(down.events, [.nav(.down)])
        XCTAssertEqual(down.forwarded.buttons, 0)
        XCTAssertEqual(step(g, Sc2Device.btnDpadDown, at: 10).events, [])
        XCTAssertEqual(step(g, Sc2Device.btnDpadDown | Sc2Device.btnA, at: 20).events, [.nav(.confirm)])
        // The stick aims by sector and reaches the game as neutral: a hard right rounds to 2.
        let aimed = step(g, 0, at: 30, lsX: 32767)
        XCTAssertEqual(aimed.events, [.nav(.sector(2))])
        XCTAssertEqual(aimed.forwarded.lsX, 0)
        // Closing hands the pad back, but a press the ring already ate plays only after a release.
        _ = step(g, Sc2Device.btnA, at: 40)
        g.ringOpen = false
        XCTAssertEqual(step(g, Sc2Device.btnA, at: 50).forwarded.buttons, 0)
        _ = step(g, 0, at: 60)
        XCTAssertEqual(step(g, Sc2Device.btnA, at: 70).forwarded.buttons, Sc2Device.btnA)
    }
}
