// ScrollCapture: the capture-side twin of the core's ScrollAccumulator — Q24.8
// quantization with per-axis residue, the Begin/Update/End lifecycle the caller's
// platform phase maps onto, source-switch cancellation, and the split of a stop that
// carries its last translation into movement-then-zero.

import PunktfunkCore
import XCTest
#if os(macOS)
import AppKit
#endif

@testable import PunktfunkKit

final class ScrollCaptureTests: XCTestCase {
    /// One emitted event as [kind, axis, delta, source, phase] — comparable as a value.
    private func wire(_ e: PunktfunkInputEvent) -> [Int64] {
        [Int64(e.kind), Int64(e.code), Int64(e.x), Int64(e.flags & 0xFF), Int64(e.flags >> 8)]
    }

    private func exp(
        _ delta: Int64, axis: Int64, _ source: PunktfunkScrollSource,
        _ phase: PunktfunkScrollPhase
    ) -> [Int64] {
        [
            Int64(PUNKTFUNK_INPUT_KIND_SCROLL.rawValue), axis, delta,
            Int64(source.rawValue), Int64(phase.rawValue),
        ]
    }

    private func events(
        _ cap: inout ScrollCapture, dx: Double, dy: Double,
        _ source: PunktfunkScrollSource, _ phase: PunktfunkScrollPhase
    ) -> [[Int64]] {
        cap.event(dx: dx, dy: dy, source: source, phase: phase).map(wire)
    }

    #if os(macOS)
    func testNativeMacSourceAndPhaseMapping() {
        let wheel = StreamLayerView.scrollWireShape(precise: false, phase: [], momentumPhase: [])!
        XCTAssertEqual(wheel.0, PUNKTFUNK_SCROLL_SOURCE_WHEEL)
        XCTAssertEqual(wheel.1, PUNKTFUNK_SCROLL_PHASE_NONE)
        let continuous = StreamLayerView.scrollWireShape(precise: true, phase: [], momentumPhase: [])!
        XCTAssertEqual(continuous.0, PUNKTFUNK_SCROLL_SOURCE_CONTINUOUS)
        for (native, wire) in [
            (NSEvent.Phase.began, PUNKTFUNK_SCROLL_PHASE_BEGIN),
            (.changed, PUNKTFUNK_SCROLL_PHASE_UPDATE),
            (.ended, PUNKTFUNK_SCROLL_PHASE_END),
            (.cancelled, PUNKTFUNK_SCROLL_PHASE_CANCEL),
        ] {
            let shape = StreamLayerView.scrollWireShape(precise: true, phase: native, momentumPhase: [])!
            XCTAssertEqual(shape.0, PUNKTFUNK_SCROLL_SOURCE_FINGER)
            XCTAssertEqual(shape.1, wire)
        }
        let momentum = StreamLayerView.scrollWireShape(precise: true, phase: .ended, momentumPhase: .began)!
        XCTAssertEqual(momentum.1, PUNKTFUNK_SCROLL_PHASE_MOMENTUM_BEGIN)
        XCTAssertNil(StreamLayerView.scrollWireShape(precise: true, phase: .mayBegin, momentumPhase: []))
    }
    #endif

    func testWheelDetentsQuantizeToQ248() {
        var cap = ScrollCapture()
        // One notch = 120 in v120 → 30720 on the wire; no gesture state, phase None.
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 120, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                   PUNKTFUNK_SCROLL_PHASE_NONE),
            [exp(120 * 256, axis: 0, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                 PUNKTFUNK_SCROLL_PHASE_NONE)])
    }

    func testResidueRidesAcrossEvents() {
        var cap = ScrollCapture()
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 0.5, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                   PUNKTFUNK_SCROLL_PHASE_NONE),
            [exp(128, axis: 0, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                 PUNKTFUNK_SCROLL_PHASE_NONE)])
        // 0.2 v120 → 51.2: the held 0.2 joins the next event instead of vanishing.
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 0.2, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                   PUNKTFUNK_SCROLL_PHASE_NONE),
            [exp(51, axis: 0, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                 PUNKTFUNK_SCROLL_PHASE_NONE)])
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 0.2, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                   PUNKTFUNK_SCROLL_PHASE_NONE),
            [exp(51, axis: 0, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                 PUNKTFUNK_SCROLL_PHASE_NONE)])
    }

    func testSmallWheelAndMomentumDeltasAccumulate() {
        for source in [PUNKTFUNK_SCROLL_SOURCE_WHEEL, PUNKTFUNK_SCROLL_SOURCE_FINGER] {
            var cap = ScrollCapture()
            let phase = source == PUNKTFUNK_SCROLL_SOURCE_WHEEL
                ? PUNKTFUNK_SCROLL_PHASE_NONE : PUNKTFUNK_SCROLL_PHASE_MOMENTUM
            let values = (0..<5).flatMap { _ in
                cap.event(dx: 0, dy: 0.2, source: source, phase: phase).map(\.x)
            }
            XCTAssertEqual(values, [51, 51, 51, 51, 52])
        }
    }

    func testZeroBeginRestartsAnOpenAxis() {
        var cap = ScrollCapture()
        _ = cap.event(dx: 0, dy: 1, source: PUNKTFUNK_SCROLL_SOURCE_FINGER,
                      phase: PUNKTFUNK_SCROLL_PHASE_BEGIN)
        XCTAssertEqual(events(&cap, dx: 0, dy: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                              PUNKTFUNK_SCROLL_PHASE_BEGIN), [
            exp(0, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER, PUNKTFUNK_SCROLL_PHASE_CANCEL),
            exp(0, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER, PUNKTFUNK_SCROLL_PHASE_BEGIN),
        ])
    }

    func testFingerGestureOpensUpdatesAndEnds() {
        var cap = ScrollCapture()
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 2, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_BEGIN),
            [exp(512, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                 PUNKTFUNK_SCROLL_PHASE_BEGIN)])
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 1, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_UPDATE),
            [exp(256, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                 PUNKTFUNK_SCROLL_PHASE_UPDATE)])
        // The end arrives carrying the last translation: movement first, zero stop after.
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 0.5, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_END),
            [
                exp(128, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                    PUNKTFUNK_SCROLL_PHASE_UPDATE),
                exp(0, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                    PUNKTFUNK_SCROLL_PHASE_END),
            ])
        // Closed now — a stale repeat of the same stop emits nothing.
        XCTAssertTrue(
            events(&cap, dx: 0, dy: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_END).isEmpty)
    }

    func testZeroDeltaNeverOpensAnAxis() {
        var cap = ScrollCapture()
        // A platform .began with no travel opens nothing; the first real delta Begins.
        XCTAssertTrue(
            events(&cap, dx: 0, dy: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_BEGIN).isEmpty)
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 1, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_UPDATE),
            [exp(256, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                 PUNKTFUNK_SCROLL_PHASE_BEGIN)])
    }

    func testSourceSwitchCancelsTheOpenAxis() {
        var cap = ScrollCapture()
        _ = events(&cap, dx: 0, dy: 1, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_BEGIN)
        // A Touch delta on the axis Finger holds: Cancel(0) under the old source, then the
        // new gesture opens with Begin.
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 1, PUNKTFUNK_SCROLL_SOURCE_TOUCH,
                   PUNKTFUNK_SCROLL_PHASE_BEGIN),
            [
                exp(0, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                    PUNKTFUNK_SCROLL_PHASE_CANCEL),
                exp(256, axis: 0, PUNKTFUNK_SCROLL_SOURCE_TOUCH,
                    PUNKTFUNK_SCROLL_PHASE_BEGIN),
            ])
    }

    func testStopFromAnotherSourceIsStale() {
        var cap = ScrollCapture()
        _ = events(&cap, dx: 0, dy: 1, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_BEGIN)
        // Touch never opened this axis — its End can't close Finger's gesture.
        XCTAssertTrue(
            events(&cap, dx: 0, dy: 0, PUNKTFUNK_SCROLL_SOURCE_TOUCH,
                   PUNKTFUNK_SCROLL_PHASE_END).isEmpty)
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 1, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_UPDATE),
            [exp(256, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                 PUNKTFUNK_SCROLL_PHASE_UPDATE)])
    }

    func testStaleMomentumEndPreservesAnotherSourcesRemainder() {
        var cap = ScrollCapture()
        _ = events(&cap, dx: 0, dy: 0.2, PUNKTFUNK_SCROLL_SOURCE_CONTROLLER, PUNKTFUNK_SCROLL_PHASE_BEGIN)
        XCTAssertTrue(events(&cap, dx: 0, dy: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                             PUNKTFUNK_SCROLL_PHASE_MOMENTUM_END).isEmpty)
        let deltas = (0..<4).flatMap { _ in
            cap.event(dx: 0, dy: 0.2, source: PUNKTFUNK_SCROLL_SOURCE_CONTROLLER,
                      phase: PUNKTFUNK_SCROLL_PHASE_UPDATE).map(\.x)
        }
        XCTAssertEqual(deltas, [51, 51, 51, 52])
    }

    func testWheelCarriesNoPhase() {
        var cap = ScrollCapture()
        // A counted source claiming a gesture boundary is rejected whole — nothing emitted,
        // no axis opened.
        XCTAssertTrue(
            events(&cap, dx: 0, dy: 1, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                   PUNKTFUNK_SCROLL_PHASE_BEGIN).isEmpty)
    }

    func testMomentumPassesThroughForGlidingSources() {
        var cap = ScrollCapture()
        _ = events(&cap, dx: 0, dy: 2, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_BEGIN)
        _ = events(&cap, dx: 0, dy: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_END)
        // The kinetic tail is its own phase run on a closed axis.
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 1.5, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_MOMENTUM_BEGIN),
            [exp(384, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                 PUNKTFUNK_SCROLL_PHASE_MOMENTUM_BEGIN)])
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_MOMENTUM_END),
            [exp(0, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                 PUNKTFUNK_SCROLL_PHASE_MOMENTUM_END)])
        // A wheel can never glide.
        XCTAssertTrue(
            events(&cap, dx: 0, dy: 1, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                   PUNKTFUNK_SCROLL_PHASE_MOMENTUM).isEmpty)
    }

    func testCancelAllClosesOpenAxes() {
        var cap = ScrollCapture()
        _ = events(&cap, dx: 1, dy: 1, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_BEGIN)
        XCTAssertEqual(
            cap.cancelAll().map(wire),
            [
                exp(0, axis: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                    PUNKTFUNK_SCROLL_PHASE_CANCEL),
                exp(0, axis: 1, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                    PUNKTFUNK_SCROLL_PHASE_CANCEL),
            ])
        XCTAssertTrue(cap.cancelAll().isEmpty)
    }

    func testNonFiniteDeltasAreDropped() {
        var cap = ScrollCapture()
        XCTAssertTrue(
            events(&cap, dx: 0, dy: .nan, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                   PUNKTFUNK_SCROLL_PHASE_NONE).isEmpty)
        XCTAssertTrue(
            events(&cap, dx: .infinity, dy: 0, PUNKTFUNK_SCROLL_SOURCE_FINGER,
                   PUNKTFUNK_SCROLL_PHASE_UPDATE).isEmpty)
        // The residue is untouched — the next finite delta quantizes alone.
        XCTAssertEqual(
            events(&cap, dx: 0, dy: 1, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                   PUNKTFUNK_SCROLL_PHASE_NONE),
            [exp(256, axis: 0, PUNKTFUNK_SCROLL_SOURCE_WHEEL,
                 PUNKTFUNK_SCROLL_PHASE_NONE)])
    }

    /// The unphased sequences Rust's `ScrollAccumulator` wrote, read from the repo: five levels
    /// up from this file is the root. A phased one re-derives its phase here, so it is skipped.
    func testMatchesTheRustVectors() throws {
        var url = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 { url.deleteLastPathComponent() }
        url.appendPathComponent("crates/core/punktfunk-core/testdata/scroll-vectors.json")
        let cases = try JSONDecoder().decode(ScrollVectors.self, from: Data(contentsOf: url)).cases
        var replayed = 0
        var wrong: [String] = []
        for c in cases where c.steps.allSatisfy({ $0.phase == 0 && $0.axis <= 1 }) {
            var cap = ScrollCapture()
            for (j, s) in c.steps.enumerated() {
                let got = events(
                    &cap, dx: s.axis == 1 ? s.delta : 0, dy: s.axis == 0 ? s.delta : 0,
                    PunktfunkScrollSource(rawValue: UInt32(s.source)),
                    PunktfunkScrollPhase(rawValue: UInt32(s.phase)))
                let want = s.wire.map {
                    [exp($0.delta, axis: $0.axis, PunktfunkScrollSource(rawValue: UInt32($0.source)),
                         PunktfunkScrollPhase(rawValue: UInt32($0.phase)))]
                } ?? []
                if got != want { wrong.append("\(c.name) step \(j): Rust \(want), Swift \(got)") }
            }
            replayed += 1
        }
        XCTAssertGreaterThan(replayed, 0)
        XCTAssertTrue(wrong.isEmpty, wrong.joined(separator: "\n"))
    }
}

private struct ScrollVectors: Decodable {
    let cases: [ScrollVectorCase]
}

private struct ScrollVectorCase: Decodable {
    let name: String
    let steps: [ScrollVectorStep]
}

private struct ScrollVectorStep: Decodable {
    let source: Int64
    let phase: Int64
    let axis: Int64
    let delta: Double
    let wire: ScrollVectorWire?
}

private struct ScrollVectorWire: Decodable {
    let source: Int64
    let phase: Int64
    let axis: Int64
    let delta: Int64
}
