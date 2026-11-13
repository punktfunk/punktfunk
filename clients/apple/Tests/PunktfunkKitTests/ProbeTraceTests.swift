import XCTest
@testable import PunktfunkKit

final class ProbeTraceTests: XCTestCase {
    private func partial(_ ms: UInt32, _ bytes: UInt64) -> PunktfunkConnection.ProbeResult {
        .init(
            done: false, recvBytes: bytes, recvPackets: 0, hostBytes: bytes, hostPackets: 0,
            elapsedMs: ms, throughputKbps: 0, lossPct: 0)
    }

    /// A round polled every 50 ms at a steady `mbps`, from its zero to `ms`.
    private func round(_ trace: inout PunktfunkConnection.ProbeTrace, mbps: Double, ms: UInt32) {
        trace.add(partial(0, 0))
        for t in stride(from: UInt32(50), through: ms, by: 50) {
            trace.add(partial(t, UInt64(mbps * 125 * Double(t)))) // Mbit/s → bytes per ms
        }
    }

    func testAPollEvery50msDrawsADenseSteadyLine() {
        var trace = PunktfunkConnection.ProbeTrace()
        round(&trace, mbps: 400, ms: 2000)
        // The first point waits for 100 ms of the round, then every poll is one.
        XCTAssertEqual(trace.points.count, 39)
        XCTAssertEqual(trace.points.first?.seconds, 0.1)
        XCTAssertEqual(trace.points.last?.seconds, 2.0)
        for point in trace.points { XCTAssertEqual(point.mbps, 400, accuracy: 0.001) }
    }

    func testAPointAveragesTheLast250msOfItsRound() {
        var trace = PunktfunkConnection.ProbeTrace()
        round(&trace, mbps: 400, ms: 500)
        // Nothing arrives for 50 ms, then the round catches up: one poll's burst.
        trace.add(partial(550, 25_000_000))
        trace.add(partial(600, 30_000_000))
        // 500…550 ms carried nothing, so the window 300…550 ms holds 200 ms of traffic.
        XCTAssertEqual(trace.points[trace.points.count - 2].mbps, 320, accuracy: 0.001)
        // 350…600 ms: 12.5 MB in 250 ms.
        XCTAssertEqual(trace.points[trace.points.count - 1].mbps, 400, accuracy: 0.001)
    }

    func testTheNextRoundContinuesTheTimeAxis() {
        var trace = PunktfunkConnection.ProbeTrace()
        round(&trace, mbps: 400, ms: 2000)
        trace.add(partial(2000, 100_000_000)) // the finished round, polled again
        round(&trace, mbps: 200, ms: 1000)
        let second = trace.points.filter { $0.seconds > 2.0001 }
        XCTAssertEqual(trace.points.count - second.count, 39)
        XCTAssertEqual(second.count, 19)
        XCTAssertEqual(second.first?.seconds ?? 0, 2.1, accuracy: 0.0001)
        XCTAssertEqual(second.last?.seconds ?? 0, 3.0, accuracy: 0.0001)
        for point in second { XCTAssertEqual(point.mbps, 200, accuracy: 0.001) }
    }

    func testAPollThatDoesNotMoveTheClockAddsNothing() {
        var trace = PunktfunkConnection.ProbeTrace()
        trace.add(partial(0, 0)) // polled before the first probe arrived
        trace.add(partial(200, 12_500_000))
        trace.add(partial(200, 12_500_000))
        XCTAssertEqual(trace.points.count, 1)
        XCTAssertEqual(trace.points[0].mbps, 500, accuracy: 0.001)
    }
}
