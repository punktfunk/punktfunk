import XCTest

#if canImport(Metal)
@testable import PunktfunkKit

/// The adaptive panel's Direct gate: in-flight accounting, and the mode switches its holds and
/// probes drive. 120 Hz panel throughout; Direct holds ~7.6 ms, a composited window ~13.2.
final class ScanoutPacerTests: XCTestCase {
    private let refresh = 1.0 / 120

    private func holds(
        _ p: ScanoutPacer, _ hold: Double, at t: Double, count: Int = ScanoutPacer.window,
        interval: Double = 1.0 / 120
    ) {
        for _ in 0..<count { p.noteDirect(hold: hold, interval: interval, now: t) }
    }

    /// A Direct frame waits for the previous present to flip: its callback, or one refresh.
    func testDirectWaitsForTheFlip() {
        let p = ScanoutPacer(now: 0)
        XCTAssertEqual(p.mode(now: 0), .direct)
        XCTAssertTrue(p.mayIssue(now: 0, minInterval: refresh))
        p.issued(at: 0, direct: true)
        XCTAssertFalse(p.mayIssue(now: 0.004, minInterval: refresh), "still in flight")
        XCTAssertTrue(p.mayIssue(now: refresh + 0.0001, minInterval: refresh), "flipped by now")
        p.issued(at: 1, direct: true)
        p.landed(issuedAt: 1)
        XCTAssertTrue(p.mayIssue(now: 1.002, minInterval: refresh), "the callback frees it sooner")
    }

    /// A booked slot present lands a refresh or two out: a Direct frame never overtakes it.
    func testDirectNeverOvertakesASlotPresent() {
        let p = ScanoutPacer(now: 0)
        p.issued(at: 10, direct: false)
        XCTAssertFalse(p.mayIssue(now: 10.02, minInterval: refresh))
        XCTAssertTrue(
            p.mayIssue(now: 10 + ScanoutPacer.staleAfter + 0.01, minInterval: refresh),
            "a lost callback cannot freeze the stream")
    }

    /// Direct holds stay under a refresh; a composited window holds ~1.6.
    func testLongHoldsSwitchToComposited() {
        let p = ScanoutPacer(now: 0)
        holds(p, 0.0132, at: 5, count: ScanoutPacer.window - 1)
        XCTAssertEqual(p.mode(now: 5), .direct, "the median needs a full window")
        p.noteDirect(hold: 0.0132, interval: refresh, now: 5)
        XCTAssertEqual(p.mode(now: 5), .composited)
        XCTAssertEqual(p.drainSwitches(), 1)

        let q = ScanoutPacer(now: 0)
        holds(q, 0.0076, at: 5)
        holds(q, 0.0132, at: 6, count: ScanoutPacer.window / 2 - 1)
        XCTAssertEqual(q.mode(now: 6), .direct, "a few long holds are not composition")
    }

    /// The hold is judged against the content interval: a composited window at 60 fps still
    /// shows every frame, so the gate stays.
    func testSlowContentKeepsTheGateWhileComposited() {
        let p = ScanoutPacer(now: 0)
        holds(p, 0.0139, at: 5, interval: 1.0 / 60)
        XCTAssertEqual(p.mode(now: 5), .direct)
        holds(p, 0.0139, at: 6, interval: refresh)
        XCTAssertEqual(p.mode(now: 6), .composited, "the same hold at 120 fps skips frames")
    }

    /// Composited probes Direct once the backoff has run.
    func testCompositedProbesAfterTheBackoff() {
        let p = ScanoutPacer(now: 0)
        holds(p, 0.0132, at: 5)
        XCTAssertEqual(p.mode(now: 5 + ScanoutPacer.baseBackoff - 0.1), .composited)
        XCTAssertEqual(p.mode(now: 5 + ScanoutPacer.baseBackoff + 0.1), .direct)
    }

    /// A probe that falls back at once doubles the wait; a lasting Direct resets it.
    func testFailedProbesBackOff() {
        let p = ScanoutPacer(now: 0)
        holds(p, 0.0132, at: 5)
        XCTAssertEqual(p.mode(now: 7.1), .direct)
        holds(p, 0.0132, at: 7.3)
        let doubled = 7.3 + 2 * ScanoutPacer.baseBackoff
        XCTAssertEqual(p.mode(now: doubled - 0.1), .composited)
        XCTAssertEqual(p.mode(now: doubled + 0.1), .direct)
        holds(p, 0.0132, at: 30)
        XCTAssertEqual(
            p.mode(now: 30 + ScanoutPacer.baseBackoff + 0.1), .direct,
            "Direct held past the probe span, so the backoff reset")
    }

    /// The backoff stops at its ceiling.
    func testBackoffIsBounded() {
        let p = ScanoutPacer(now: 0)
        var t = 5.0
        holds(p, 0.0132, at: t)
        for _ in 0..<10 {
            t += ScanoutPacer.maxBackoff + 0.1
            XCTAssertEqual(p.mode(now: t), .direct)
            holds(p, 0.0132, at: t + 0.1)
            t += 0.1
        }
        XCTAssertEqual(p.mode(now: t + ScanoutPacer.maxBackoff + 0.1), .direct)
    }

    /// A window the app knows cannot be Direct keeps the holds' verdict and never re-probes on
    /// the timer; one that can again probes at once.
    func testDirectPossibleGatesTheTimedProbes() {
        let p = ScanoutPacer(now: 0)
        p.setDirectPossible(false, now: 1)
        XCTAssertEqual(p.mode(now: 1.01), .direct, "the holds decide, not the flag")
        holds(p, 0.0132, at: 2)
        XCTAssertEqual(p.mode(now: 2 + ScanoutPacer.maxBackoff + 1), .composited, "no probe")
        p.setDirectPossible(false, now: 40)
        XCTAssertEqual(p.drainSwitches(), 1, "a repeat is a no-op")
        p.setDirectPossible(true, now: 50)
        XCTAssertEqual(p.mode(now: 50.01), .direct)

        let q = ScanoutPacer(now: 0)
        holds(q, 0.0132, at: 5)
        q.setDirectPossible(true, now: 5.01)
        XCTAssertEqual(q.mode(now: 5.02), .composited, "already possible: the backoff holds")
        q.setDirectPossible(false, now: 5.03)
        q.setDirectPossible(true, now: 5.04)
        XCTAssertEqual(q.mode(now: 5.05), .direct, "a change to possible probes at once")
    }

    /// 120 fps on a 120 Hz panel whose first 0.5 s composites (presents hold 1.6 refreshes, as
    /// during the fullscreen animation), then scans out Direct (2 ms). The pacer is never told
    /// "impossible" in fullscreen, so it falls to the slot and probes back to Direct for good.
    func testSessionOpeningCompositedReturnsToDirect() {
        let p = ScanoutPacer(now: 0)
        let settled = 0.5
        var callbacks: [(at: Double, issued: Double, direct: Bool)] = []
        var mode = ScanoutPacer.Mode.direct
        var fell: Double?
        var directAgain: Double?
        var t = 0.0
        while t < 10 {
            callbacks.sort { $0.at < $1.at }
            while let c = callbacks.first, c.at <= t {
                callbacks.removeFirst()
                p.landed(issuedAt: c.issued)
                if c.direct { p.noteDirect(hold: c.at - c.issued, interval: refresh, now: c.at) }
            }
            let now = p.mode(now: t)
            if now != mode {
                mode = now
                if now == .composited { fell = fell ?? t } else { directAgain = t }
            }
            if now == .composited {
                p.issued(at: t, direct: false)
                callbacks.append((t + refresh, t, false))
            } else if p.mayIssue(now: t, minInterval: refresh) {
                p.issued(at: t, direct: true)
                callbacks.append((t + (t < settled ? 1.6 * refresh : 0.002), t, true))
            }
            t += refresh
        }
        XCTAssertLessThan(try XCTUnwrap(fell), settled, "the opening holds fall to the slot")
        XCTAssertNotNil(directAgain, "the gate probes Direct again once the window can")
        XCTAssertEqual(mode, .direct)
        XCTAssertEqual(p.drainSwitches(), 2, "one fall, one probe that holds")
    }
}
#endif
