// SpeedTestRun.reduce: a row per thing Start, Stop and the probe do to the speed test page.

import XCTest

@testable import PunktfunkKit

final class SpeedTestRunTests: XCTestCase {
    private typealias Run = SpeedTestRun

    private static let report = PunktfunkConnection.HealthReport(
        ceilingKbps: 850_000, wall: true,
        clean: PunktfunkConnection.CleanRound(rateKbps: 425_000, lossPct: 0.2, jitterUs: 900),
        clientIfaceKind: 1, clientLinkMbps: 1000, clientRcvbufKb: 4096, hostIfaceKind: 1,
        hostLinkMbps: 1000, hostSndbufKb: 4096, legLossPct: [], findings: [])

    private func after(_ events: [Run.Event], from state: Run.State = Run.State()) -> Run.State {
        events.reduce(state, Run.reduce)
    }

    func testAProbeConnectsMeasuresAndAnswers() {
        XCTAssertEqual(after([.start]).phase, .connecting)
        XCTAssertEqual(after([.start, .connected]).phase, .probing)
        let done = after([.start, .connected, .checked(Self.report)])
        XCTAssertEqual(done.phase, .done(Run.probeResult(from: Self.report)))
        XCTAssertEqual(done.report, Self.report)
    }

    func testAProbeThatMeasuredNothingFails() {
        XCTAssertEqual(
            after([.start, .connected, .checked(nil)]).phase,
            .failed(String(localized: L10n.speedtestNoResultBody)))
    }

    func testAHostThatNeverAnswersFails() {
        XCTAssertEqual(after([.start, .unreachable("gone")]).phase, .failed("gone"))
    }

    func testStopDropsTheProbesLateWord() {
        XCTAssertEqual(after([.start, .stop]).phase, .idle)
        XCTAssertEqual(after([.start, .stop, .connected]).phase, .idle)
        XCTAssertEqual(after([.start, .connected, .stop, .checked(Self.report)]), Run.State())
        XCTAssertEqual(after([.start, .stop, .unreachable("gone")]).phase, .idle)
    }

    func testRunAgainClearsTheLastReport() {
        let again = after([.start, .connected, .checked(Self.report), .start])
        XCTAssertEqual(again.phase, .connecting)
        XCTAssertNil(again.report)
    }

    /// The measurement is the ceiling the ramp proved, with the clean round's loss.
    func testTheReportBecomesTheMeasurement() {
        let result = Run.probeResult(from: Self.report)
        XCTAssertEqual(result.throughputKbps, 850_000)
        XCTAssertEqual(result.lossPct, 0.2)
        XCTAssertTrue(result.done)
    }
}
