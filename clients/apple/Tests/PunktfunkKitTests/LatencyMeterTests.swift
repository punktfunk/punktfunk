// Unit tests for LatencyMeter: the median, reset-on-drain, the absurd-value guard, and the
// explicit-instant stage form (record(ptsNs:atNs:offsetNs:), used for the client-local
// decode/display stages and the at-present end-to-end stamp). Receipt-path
// latencies are constructed by stamping a pts a known interval in the past, so the result is that
// interval plus the (tiny) clock advance between reads — asserted with tolerance; the explicit
// form is exact.

import Foundation
import XCTest

@testable import PunktfunkKit

final class LatencyMeterTests: XCTestCase {
    private func nowRealtimeNs() -> UInt64 {
        UInt64(realtimeNowNs())
    }

    func testEmptyDrainIsNil() {
        XCTAssertNil(LatencyMeter().drain())
    }

    func testRecordsTheMedianAndResets() {
        let m = LatencyMeter()
        let now = nowRealtimeNs()
        // Each frame "captured" 5 ms ago, no skew offset → latency ≈ 5 ms.
        for _ in 0..<50 { m.record(ptsNs: now - 5_000_000, offsetNs: 0) }
        guard let s = m.drain() else { return XCTFail("expected samples") }
        XCTAssertEqual(s.p50Ms, 5.0, accuracy: 2.0)
        XCTAssertNil(m.drain(), "drain resets the window")
    }

    func testExplicitStageRecordIsExact() {
        let m = LatencyMeter()
        // A client-local stage (decode: received→decoded) — start instant as ptsNs, offset 0.
        let receivedNs: Int64 = 1_000_000_000_000
        m.record(ptsNs: UInt64(receivedNs), atNs: receivedNs + 3_000_000, offsetNs: 0)
        XCTAssertEqual(m.drain()?.p50Ms, 3.0, "explicit instants make the sample exact")
    }

    func testExplicitStageDropsNonPositiveInterval() {
        let m = LatencyMeter()
        // A stage whose start stamp is missing (0) or after its end must not pollute the window.
        let decodedNs: Int64 = 1_000_000_000_000
        m.record(ptsNs: 0, atNs: decodedNs, offsetNs: 0) // "start unknown" → > 10 s → dropped
        m.record(ptsNs: UInt64(decodedNs + 1), atNs: decodedNs, offsetNs: 0) // negative → dropped
        XCTAssertNil(m.drain())
    }

    func testDropsAbsurdValues() {
        let m = LatencyMeter()
        let now = nowRealtimeNs()
        // pts 1 s in the future → negative latency → dropped.
        m.record(ptsNs: now + 1_000_000_000, offsetNs: 0)
        // pts absurdly far in the past → > 10 s latency → dropped.
        m.record(ptsNs: now - 20_000_000_000, offsetNs: 0)
        XCTAssertNil(m.drain())
    }

    // MARK: - latestSample: the A/V sync loop's video reference

    /// The end-to-end meter doubles as the reference the audio ring steers against, so its most
    /// recent sample must be readable as a LEVEL — without consuming it, and independently of the
    /// 1 Hz percentile window the HUD drains.
    func testLatestSampleSurvivesDrainAndIsNotAWindow() {
        let m = LatencyMeter()
        let atNs: Int64 = 1_000_000_000_000
        m.record(ptsNs: UInt64(atNs - 12_000_000), atNs: atNs, offsetNs: 0) // 12 ms
        XCTAssertEqual(m.latestSample(asOfNs: atNs, maxAgeMs: 500), 12_000_000)
        _ = m.drain()
        XCTAssertEqual(
            m.latestSample(asOfNs: atNs, maxAgeMs: 500), 12_000_000,
            "the reference is a level — draining the percentile window must not clear it")
        // …and it tracks the newest frame.
        m.record(ptsNs: UInt64(atNs - 20_000_000), atNs: atNs, offsetNs: 0)
        XCTAssertEqual(m.latestSample(asOfNs: atNs, maxAgeMs: 500), 20_000_000)
    }

    /// No frame yet ⇒ no reference. This is what keeps the sync loop inert at session start.
    func testLatestSampleIsNilBeforeAnyFrame() {
        XCTAssertNil(LatencyMeter().latestSample(asOfNs: 1_000_000_000_000, maxAgeMs: 500))
    }

    /// THE staleness gate: video can stop while audio keeps playing (the backgrounded keep-alive
    /// drops decode entirely). A level with no expiry would go on offering a minutes-old figure as
    /// though it were live, and the ring would be steered against a frozen reference.
    func testLatestSampleExpires() {
        let m = LatencyMeter()
        let atNs: Int64 = 1_000_000_000_000
        m.record(ptsNs: UInt64(atNs - 12_000_000), atNs: atNs, offsetNs: 0)
        XCTAssertNotNil(m.latestSample(asOfNs: atNs + 499_000_000, maxAgeMs: 500))
        XCTAssertNil(
            m.latestSample(asOfNs: atNs + 501_000_000, maxAgeMs: 500),
            "a stale reference must read as NO reference, not as a live one")
        // A stamp marginally ahead of the reader's clock is normal (the deadline presenter stamps
        // at the link's TARGET present time) and must not drop the only reference we have.
        XCTAssertNotNil(m.latestSample(asOfNs: atNs - 8_000_000, maxAgeMs: 500))
    }

    /// A sample the meter refused must not become a reference either — the sync loop would then be
    /// steered by a value the percentile window itself judged absurd.
    ///
    /// The ABSURDLY LARGE case is the load-bearing one: a negative interval would also be stopped
    /// by `latestSample`'s own `> 0` check, so on its own it proves nothing about where the publish
    /// sits relative to the guard.
    func testRefusedSampleIsNotPublishedAsAReference() {
        let m = LatencyMeter()
        let atNs: Int64 = 1_000_000_000_000
        m.record(ptsNs: UInt64(atNs - 20_000_000_000), atNs: atNs, offsetNs: 0) // 20 s → refused
        XCTAssertNil(
            m.latestSample(asOfNs: atNs, maxAgeMs: 500),
            "a sample too absurd for the window is too absurd to steer the ring")
        m.record(ptsNs: UInt64(atNs + 1), atNs: atNs, offsetNs: 0) // negative interval
        XCTAssertNil(m.latestSample(asOfNs: atNs, maxAgeMs: 500))
        // …and a good sample after them still lands, so the refusals cost nothing.
        m.record(ptsNs: UInt64(atNs - 9_000_000), atNs: atNs, offsetNs: 0)
        XCTAssertEqual(m.latestSample(asOfNs: atNs, maxAgeMs: 500), 9_000_000)
    }

    func testAnUndrainedMeterStopsGrowingButKeepsItsLevel() {
        let m = LatencyMeter()
        let atNs: Int64 = 1_700_000_000_000_000_000
        // 4096 samples fill the window; the 5000 after them reach only the level.
        for _ in 0..<4096 { m.record(ptsNs: UInt64(atNs - 9_000_000), atNs: atNs, offsetNs: 0) }
        for _ in 0..<5000 { m.record(ptsNs: UInt64(atNs - 1_000_000), atNs: atNs, offsetNs: 0) }
        XCTAssertEqual(m.latestSample(asOfNs: atNs, maxAgeMs: 500), 1_000_000)
        XCTAssertEqual(m.drain()?.p50Ms, 9.0, "samples past the cap stayed out of the window")
    }
}
