import XCTest
@testable import PunktfunkKit

final class ProbeTraceTests: XCTestCase {
    private func partial(_ ms: UInt32, _ bytes: UInt64) -> PunktfunkConnection.ProbeResult {
        .init(
            done: false, recvBytes: bytes, recvPackets: 0, hostBytes: bytes, hostPackets: 0,
            elapsedMs: ms, throughputKbps: 0, lossPct: 0)
    }

    /// A round polled every `step` ms at a steady `mbps`, from its zero to `ms`.
    private func round(
        _ trace: inout PunktfunkConnection.ProbeTrace, mbps: Double, ms: UInt32, step: UInt32 = 20
    ) {
        trace.add(partial(0, 0))
        for t in stride(from: step, through: ms, by: Int(step)) {
            trace.add(partial(t, UInt64(mbps * 125 * Double(t)))) // Mbit/s → bytes per ms
        }
    }

    func testAPollEvery20msDrawsADenseSteadyLine() {
        var trace = PunktfunkConnection.ProbeTrace()
        round(&trace, mbps: 400, ms: 2000)
        // The first point waits for 100 ms of traffic, then every poll is one.
        XCTAssertEqual(trace.points.count, 96)
        XCTAssertEqual(trace.points.first?.seconds, 0.1)
        XCTAssertEqual(trace.points.last?.seconds, 2.0)
        for point in trace.points { XCTAssertEqual(point.mbps, 400, accuracy: 0.001) }
    }

    func testAStepIsSmoothedWithA200msTimeConstant() {
        var trace = PunktfunkConnection.ProbeTrace()
        round(&trace, mbps: 400, ms: 1000)
        // The rate halves. One time constant later the curve has covered 1 − 1/e of the step.
        var bytes: UInt64 = 50_000_000
        for t in stride(from: UInt32(1020), through: 1200, by: 20) {
            bytes += 500_000 // 20 ms at 200 Mbit/s
            trace.add(partial(t, bytes))
        }
        XCTAssertEqual(trace.points.last?.mbps ?? 0, 200 + 200 / M_E, accuracy: 0.01)
        // No point jumps: each moves less than a tenth of the step.
        for (a, b) in zip(trace.points, trace.points.dropFirst()) {
            XCTAssertLessThan(abs(a.mbps - b.mbps), 20)
        }
    }

    func testTheNextRoundContinuesTheTimeAxisAndTheCurve() {
        var trace = PunktfunkConnection.ProbeTrace()
        round(&trace, mbps: 400, ms: 2000)
        trace.add(partial(2000, 100_000_000)) // the finished round, polled again
        round(&trace, mbps: 200, ms: 1000)
        let second = trace.points.filter { $0.seconds > 2.0001 }
        XCTAssertEqual(trace.points.count - second.count, 96)
        XCTAssertEqual(second.first?.seconds ?? 0, 2.02, accuracy: 0.0001)
        XCTAssertEqual(second.last?.seconds ?? 0, 3.0, accuracy: 0.0001)
        // The curve carries over from 400 and settles on the new round's rate.
        XCTAssertGreaterThan(second.first?.mbps ?? 0, 350)
        XCTAssertEqual(second.last?.mbps ?? 0, 200, accuracy: 2)
    }

    func testAPollThatDoesNotMoveTheClockAddsNothing() {
        var trace = PunktfunkConnection.ProbeTrace()
        trace.add(partial(0, 0)) // polled before the first probe arrived
        trace.add(partial(200, 12_500_000))
        trace.add(partial(200, 12_500_000))
        XCTAssertEqual(trace.points.count, 1)
        XCTAssertEqual(trace.points[0].mbps, 500, accuracy: 0.001)
    }

    func testTheLiveNumberStepsEvery200ms() {
        var trace = PunktfunkConnection.ProbeTrace()
        XCTAssertNil(trace.live())
        round(&trace, mbps: 400, ms: 500)
        var bytes: UInt64 = 25_000_000
        for t in stride(from: UInt32(520), through: 700, by: 20) {
            bytes += 500_000 // 200 Mbit/s from here
            trace.add(partial(t, bytes))
        }
        // At 0.70 s the number shows the curve as it stood at 0.60 s, not the newest poll.
        let at600 = trace.points.first { abs($0.seconds - 0.6) < 0.0001 }?.mbps
        XCTAssertEqual(trace.live(), at600)
        XCTAssertNotEqual(trace.live(), trace.points.last?.mbps)
    }
}
