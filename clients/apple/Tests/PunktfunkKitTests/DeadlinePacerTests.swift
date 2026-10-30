import XCTest

#if canImport(Metal)
import CoreVideo
import QuartzCore
@testable import PunktfunkKit

/// Deadline pacing: pick inside the window before Apple's deadline less the measured margin,
/// never present past the cutoff, learn from late presents at the cutoff, drain a queue once.
final class DeadlinePacerTests: XCTestCase {
    private let targetNs: Int64 = 1_000_000_000

    private func pair(
        now: Double, pick: Double = 1.007, latePick: Bool = true, drain: Bool = false,
        last: Double = 1.009
    ) -> DeadlinePacer.Pair {
        DeadlinePacer.pair(
            now: now, vendAt: 1.000, last: last, pick: pick, latePick: latePick, drain: drain)
    }

    private func pickAt(_ p: DeadlinePacer.Pair) -> Double {
        if case .present(let at) = p { return at }
        return -1
    }

    func testTheLatencyIntentWaitsForThePickOrPresentsALateArrivalAtOnce() {
        XCTAssertEqual(pickAt(pair(now: 1.001)), 1.007, accuracy: 1e-9)
        XCTAssertEqual(pickAt(pair(now: 1.008)), 1.008, accuracy: 1e-9, "past the pick")
    }

    func testSmoothnessPresentsOnPairing() {
        XCTAssertEqual(pickAt(pair(now: 1.001, latePick: false)), 1.001, accuracy: 1e-9)
    }

    func testPastTheCutoffOrDrainingSkips() {
        XCTAssertEqual(pair(now: 1.0095), .skip, "would land a refresh late")
        XCTAssertEqual(pair(now: 1.001, drain: true), .skip)
    }

    /// A margin past the whole window must leave a quarter of it: expiring every drawable
    /// would freeze the stream, and with nothing rendered the margin could never come down.
    func testTheCutoffAlwaysLeavesAQuarterOfTheWindow() {
        XCTAssertEqual(
            DeadlinePacer.cutoff(vendAt: 1.000, deadline: 1.008, margin: 0.004), 1.004,
            accuracy: 1e-9)
        XCTAssertEqual(
            DeadlinePacer.cutoff(vendAt: 1.000, deadline: 1.008, margin: 0.0094), 1.002,
            accuracy: 1e-9)
    }

    func testNoWindowAfterTheVendPresentsAtOnce() {
        XCTAssertEqual(pickAt(pair(now: 1.002, last: 0.999)), 1.002, accuracy: 1e-9)
    }

    /// Before issue → GPU done has samples, encode + GPU busy stand in for it.
    func testTheMarginFallsBackToEncodeAndGpuBusy() {
        let pacer = DeadlinePacer()
        XCTAssertEqual(pacer.margin, 0.001, accuracy: 1e-9, "floor before any sample")
        pacer.noteRender(seconds: 0.0004)
        pacer.noteGPU(issuedAt: 1.0, start: 1.0006, end: 1.0055, deadline: 1.008)
        pacer.noteHeld(wakeLate: 0.001, superseded: false)
        XCTAssertEqual(pacer.margin, 0.001 + 0.0004 + 0.0049 + 0.0005, accuracy: 1e-9)
        XCTAssertTrue(pacer.windowLine().text.contains(" gpuMs=4.90 renderMs=-1.00 "))
    }

    /// The deadline is when the frame must be done: the margin holds issue → GPU done at p99,
    /// which already holds the encode. One spike leaves it where it was.
    func testTheMarginTracksIssueToGpuDone() {
        let pacer = DeadlinePacer()
        pacer.noteRender(seconds: 0.0006)
        pacer.noteHeld(wakeLate: 0.0003, superseded: false)
        for i in 0..<RecentP99.capacity {
            let span = 0.002 + Double(i % 5) * 0.0001 // 2.0…2.4 ms
            pacer.noteGPU(issuedAt: 1.0, start: 1.0008, end: 1.0 + span, deadline: 1.008)
        }
        XCTAssertEqual(pacer.margin, 0.0003 + 0.0024 + 0.0005, accuracy: 1e-9)
        XCTAssertTrue(pacer.windowLine().text.contains(" renderMs=2.40 "))
        pacer.noteGPU(issuedAt: 1.0, start: 1.0008, end: 1.007, deadline: 1.008)
        XCTAssertEqual(pacer.margin, 0.0003 + 0.0024 + 0.0005, accuracy: 1e-9, "a 7 ms spike")
    }

    func testTheSpanTakesOverOnceItHasSamples() {
        var p99 = RecentP99()
        for _ in 1..<RecentP99.minimum { p99.note(0.002) }
        XCTAssertNil(p99.value)
        p99.note(0.002)
        XCTAssertEqual(p99.value ?? 0, 0.002, accuracy: 1e-12)
    }

    /// `queued`: a later present went out before this one reached glass.
    private func present(
        _ pacer: DeadlinePacer, issuedNs: Int64, lateMs: Double?, cutoffLeadMs: Double = 9,
        queued: Bool = true
    ) {
        if queued { pacer.noteIssued(ns: issuedNs + 8_333_333) }
        pacer.observe(
            issuedNs: issuedNs, presentedNs: lateMs.map { targetNs + Int64($0 * 1e6) },
            targetNs: targetNs, cutoffLeadNs: Int64(cutoffLeadMs * 1e6))
    }

    /// Issued 9.5 ms before the target: within 1 ms of a 9 ms cutoff lead.
    private let atCutoff = Int64(1_000_000_000 - 9_500_000)

    /// One learning window, its first `late` presents a refresh late with nothing queued
    /// behind, so each is its own event.
    private func window(_ pacer: DeadlinePacer, late: Int, issuedNs: Int64? = nil) {
        for i in 0..<DeadlinePacer.learnWindow {
            present(
                pacer, issuedNs: issuedNs ?? atCutoff, lateMs: i < late ? 8.3 : 0,
                queued: false)
        }
    }

    func testAnIsolatedLatePresentDrainsOnceWithoutLearning() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        present(pacer, issuedNs: atCutoff, lateMs: 8.3)
        present(pacer, issuedNs: atCutoff, lateMs: 8.3) // same event, still undrained
        XCTAssertEqual(pacer.margin, base, accuracy: 1e-9, "the system's own odd late present")
        XCTAssertTrue(pacer.takeDrain(nowNs: 2_000_000_000))
        XCTAssertFalse(pacer.takeDrain(nowNs: 2_000_000_001), "one skip per event")
    }

    /// No later present waits behind it, so skipping a vend would only delay the next frame.
    func testALatePresentWithNothingQueuedArmsNoDrain() {
        let pacer = DeadlinePacer()
        present(pacer, issuedNs: atCutoff, lateMs: 8.3, queued: false)
        XCTAssertFalse(pacer.takeDrain(nowNs: 2_000_000_000))
    }

    /// Late presents more margin does not cure: the bump fails its trial, and learning rests.
    func testLatePresentsThatIgnoreTheMarginUndoTheBumpThenRest() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        window(pacer, late: 12)
        XCTAssertEqual(pacer.margin, base + 0.001, accuracy: 1e-9, "on trial")
        window(pacer, late: 12)
        XCTAssertEqual(pacer.margin, base, accuracy: 1e-9, "the same rate: undone")
        for _ in 0..<10 { window(pacer, late: 12) }
        XCTAssertEqual(pacer.margin, base, accuracy: 1e-9, "resting")
        XCTAssertTrue(pacer.windowLine().text.contains(" bumps=1 undone=1 "))
    }

    /// Late presents each 1 ms cuts by more than half: every bump stays. The decay probes the
    /// extra, and the late presents its loss brings learn it back.
    func testLatePresentsTheMarginCuresKeepTheirBumps() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        let source = { () -> Int in
            let extra = pacer.margin - base
            return extra < 0.00099 ? 24 : extra < 0.00199 ? 8 : 0
        }
        for _ in 0..<3 { window(pacer, late: source()) }
        XCTAssertEqual(pacer.margin, base + 0.002, accuracy: 1e-9)
        for _ in 0..<30 {
            window(pacer, late: source())
            XCTAssertGreaterThanOrEqual(pacer.margin - base, 0.0015 - 1e-9)
            XCTAssertLessThanOrEqual(pacer.margin - base, 0.0025 + 1e-9)
        }
    }

    /// A steady background under the bump count cannot hold an extra that no longer pays.
    func testABackgroundOfLatePresentsCannotBlockTheDecay() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        window(pacer, late: 24)
        window(pacer, late: 0)
        XCTAssertEqual(pacer.margin, base + 0.001, accuracy: 1e-9)
        for _ in 0..<4 { window(pacer, late: 5) }
        XCTAssertEqual(pacer.margin, base + 0.0005, accuracy: 1e-9)
        for _ in 0..<5 { window(pacer, late: 5) }
        XCTAssertEqual(pacer.margin, base, accuracy: 1e-9)
    }

    func testLatePresentsWellBeforeTheCutoffNeverLearn() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        window(pacer, late: 24, issuedNs: targetNs - 16_000_000)
        XCTAssertEqual(pacer.margin, base, accuracy: 1e-9, "a queue, not a short margin")
    }

    func testPresentsQueuedBeforeTheDrainCountForNothing() {
        let pacer = DeadlinePacer()
        present(pacer, issuedNs: atCutoff, lateMs: 8.3)
        XCTAssertTrue(pacer.takeDrain(nowNs: atCutoff + 5_000_000))
        present(pacer, issuedNs: atCutoff + 1_000_000, lateMs: 8.3) // queued before the skip
        XCTAssertFalse(pacer.takeDrain(nowNs: atCutoff + 9_000_000))
    }

    func testADroppedPresentArmsNothing() {
        let pacer = DeadlinePacer()
        present(pacer, issuedNs: atCutoff, lateMs: nil)
        XCTAssertFalse(pacer.takeDrain(nowNs: 2_000_000_000))
    }

    /// Release builds log the window line only when it holds a late or dropped present.
    func testOnlyALateOrDroppedPresentTroublesTheWindow() {
        let pacer = DeadlinePacer()
        present(pacer, issuedNs: atCutoff, lateMs: 0)
        XCTAssertFalse(pacer.windowLine().troubled)
        present(pacer, issuedNs: atCutoff, lateMs: 8.3)
        XCTAssertTrue(pacer.windowLine().troubled)
        XCTAssertFalse(pacer.windowLine().troubled, "the line resets its window")
        present(pacer, issuedNs: atCutoff, lateMs: nil)
        XCTAssertTrue(pacer.windowLine().troubled)
    }

    /// The late line's rate limit: each late present's count in its window line.
    func testObserveCountsLatePresentsPerWindowLine() {
        let pacer = DeadlinePacer()
        let observe = { (presentedNs: Int64?) in
            pacer.observe(
                issuedNs: self.atCutoff, presentedNs: presentedNs, targetNs: self.targetNs,
                cutoffLeadNs: 9_000_000)
        }
        XCTAssertEqual(observe(targetNs), 0, "on time")
        XCTAssertEqual(observe(nil), 0, "dropped")
        for n in 1...3 { XCTAssertEqual(observe(targetNs + 8_300_000), n) }
        _ = pacer.windowLine()
        XCTAssertEqual(observe(targetNs + 8_300_000), 1, "a new window line starts over")
    }

    func testGpuPastTheDeadlineAndPresentsInFlightAreCounted() {
        let pacer = DeadlinePacer()
        pacer.noteGPU(issuedAt: 1.000, start: 1.000, end: 1.004, deadline: 1.008)
        pacer.noteGPU(issuedAt: 1.005, start: 1.006, end: 1.009, deadline: 1.008)
        XCTAssertTrue(pacer.windowLine().text.contains(" gpuPastDeadline=1 "))
        XCTAssertTrue(pacer.windowLine().text.contains(" gpuPastDeadline=0 "), "per window")
        pacer.noteIssued(ns: 1)
        pacer.noteIssued(ns: 2)
        XCTAssertEqual(pacer.inFlight, 2)
        present(pacer, issuedNs: 1, lateMs: 0, queued: false)
        XCTAssertEqual(pacer.inFlight, 1)
    }

    func testTheLateLineNamesEveryFieldAgainstTheDeadline() {
        let late = LatePresent(
            lateNs: 8_330_000, vendAt: 10.000, issuedAt: 10.0015, deadline: 10.0083,
            gpu: (start: 10.0016, end: 10.0091), held: true, carried: false, after: "expired",
            inFlight: 2, decodeToIssueNs: 3_250_000)
        XCTAssertEqual(
            late.line,
            "pf-late lateMs=8.33 issueToDeadlineMs=6.80 gpuStartMs=-6.70 gpuEndMs=0.80 held=1 "
                + "carried=0 after=expired inFlight=2 ageMs=1.50 decodeToIssueMs=3.25")
        let bare = LatePresent(
            lateNs: 8_330_000, vendAt: 10.000, issuedAt: 10.0015, deadline: 10.0083, gpu: nil,
            held: false, carried: true, after: "presented", inFlight: 0, decodeToIssueNs: nil)
        XCTAssertTrue(bare.line.contains(" gpuStartMs=na gpuEndMs=na "))
        XCTAssertTrue(bare.line.hasSuffix(" decodeToIssueMs=na"))
    }
}

/// Where frames land in the refresh, and the pick that keeps two of them out of one refresh.
final class PhaseTrackerTests: XCTestCase {
    private let p = 0.00833
    private let w = 0.0065

    private func settled(at f: Double) -> PhaseTracker {
        var phase = PhaseTracker()
        for i in 0..<PhaseTracker.capacity { phase.note(fraction: f + (i % 2 == 0 ? 0.005 : -0.005)) }
        return phase
    }

    func testTheEstimateWrapsAroundTheVend() {
        var phase = PhaseTracker()
        XCTAssertNil(phase.estimate())
        for i in 0..<PhaseTracker.capacity { phase.note(fraction: i % 2 == 0 ? 0.98 : 1.02) }
        let est = phase.estimate()
        XCTAssertNotNil(est)
        XCTAssertEqual(min(est!.mean, 1 - est!.mean), 0, accuracy: 0.01, "0.98 and 0.02 meet at 0")
        XCTAssertEqual(est!.sigma, 0.02 / 1.645, accuracy: 0.003)
    }

    func testAnOutlierIsNotSampledButAMovedPhaseRestarts() {
        var phase = settled(at: 0.3)
        phase.note(fraction: 0.8)
        XCTAssertEqual(phase.estimate()!.mean, 0.3, accuracy: 0.005, "one loss burst")
        XCTAssertEqual(phase.drainCounters().rejected, 1)
        for _ in 0..<PhaseTracker.capacity / 4 { phase.note(fraction: 0.8) }
        XCTAssertNil(phase.estimate(), "a half second of nothing else: the phase moved")
    }

    func testAnEarlyArrivalIsPickedMidWayAfterIt() {
        var phase = PhaseTracker()
        let pick = phase.pick(arrival: 0.001, sigma: 0.0002, period: p, window: w, now: 0)
        XCTAssertEqual(pick.offset, 0.001 + p / 2, accuracy: 1e-9)
        XCTAssertFalse(pick.switched)
        XCTAssertEqual(phase.mapping, .same)
    }

    func testAnArrivalTooCloseToTheWindowEndWaitsARefresh() {
        var phase = PhaseTracker()
        let pick = phase.pick(arrival: 0.0055, sigma: 0.0005, period: p, window: w, now: 0)
        XCTAssertEqual(pick.offset, 0.0055 - p / 2, accuracy: 1e-9, "mid-way after the previous")
        XCTAssertTrue(pick.switched)
        XCTAssertEqual(phase.mapping, .next)
    }

    func testAClearedSamePickNeverTradesARefreshForMoreClearance() {
        var phase = PhaseTracker()
        // Same clears by 2.5 ms (≥ 3σ); next would clear by 4 ms but costs a refresh.
        let pick = phase.pick(arrival: 0.004, sigma: 0.0005, period: p, window: w, now: 0)
        XCTAssertEqual(pick.offset, w, accuracy: 1e-9)
        XCTAssertEqual(phase.mapping, .same)
    }

    func testTheWayBackWaitsOutTheDwell() {
        var phase = PhaseTracker()
        _ = phase.pick(arrival: 0.0055, sigma: 0.0005, period: p, window: w, now: 0)
        XCTAssertEqual(phase.mapping, .next)
        _ = phase.pick(arrival: 0.004, sigma: 0.0005, period: p, window: w, now: 1)
        XCTAssertEqual(phase.mapping, .next, "a switch costs a glitch; 1 s is too soon")
        let back = phase.pick(arrival: 0.004, sigma: 0.0005, period: p, window: w, now: 2.5)
        XCTAssertEqual(phase.mapping, .same)
        XCTAssertTrue(back.switched)
    }

    func testAPickInsideOneSigmaSwitchesAtOnce() {
        var phase = PhaseTracker()
        _ = phase.pick(arrival: 0.0055, sigma: 0.0005, period: p, window: w, now: 0)
        let pick = phase.pick(arrival: 0.0002, sigma: 0.0005, period: p, window: w, now: 0.5)
        XCTAssertTrue(pick.switched, "frames are flipping now")
        XCTAssertEqual(phase.mapping, .same)
    }
}

/// The bounded one-frame hold.
final class CarryTests: XCTestCase {
    func testABurstShowsTheOlderAndKeepsTheNewer() {
        var carry = Carry()
        XCTAssertTrue(carry.choose([1], allowed: true) == (1, nil))
        XCTAssertTrue(carry.choose([1, 2], allowed: true) == (1, 2))
        XCTAssertTrue(carry.choose([2, 3], allowed: true) == (2, 3), "one behind while it lasts")
        XCTAssertTrue(carry.choose([3], allowed: true) == (3, nil), "a gap ends the hold")
        XCTAssertEqual(carry.run, 0)
        XCTAssertEqual(carry.drainCounters().carried, 2)
    }

    func testTwoNewBehindAKeptFrameDropTheKeptOne() {
        var carry = Carry()
        XCTAssertTrue(carry.choose([1, 2, 3], allowed: true) == (2, 3))
    }

    func testAPhaseSwitchNeverCarries() {
        var carry = Carry()
        XCTAssertTrue(carry.choose([1, 2], allowed: false) == (2, nil))
    }

    func testAStandingQueueEndsAfterItsBound() {
        var carry = Carry()
        for i in 0..<Carry.maxRun { _ = carry.choose([i, i + 1], allowed: true) }
        XCTAssertTrue(carry.choose([50, 51], allowed: true) == (51, nil))
        XCTAssertEqual(carry.drainCounters().forcedDrains, 1)
    }

    /// A host slower than the panel ends its own holds within a beat: the bound covers one.
    func testASlowHostHoldsForABeat() {
        let p = 1.0 / 120
        XCTAssertEqual(Carry.run(forSlowBy: 0, period: p), Carry.maxRun, "matched or faster")
        XCTAssertEqual(Carry.run(forSlowBy: 2.3, period: p), Int(1.2 / 2.3 / p))
        XCTAssertEqual(Carry.run(forSlowBy: 0.4, period: p), Int(2.0 / p), "capped at 2 s")
        XCTAssertEqual(Carry.run(forSlowBy: 60, period: p), Carry.maxRun, "never under the base")
        var carry = Carry()
        for i in 0..<Carry.maxRun { _ = carry.choose([i, i + 1], allowed: true, maxRun: 62) }
        XCTAssertTrue(carry.choose([50, 51], allowed: true, maxRun: 62) == (50, 51), "still held")
    }

    func testTheStoreKeepsTheNewestTwo() {
        let store = FrameStore<Int>(policy: .newestTwo)
        store.submit(1); store.submit(2); store.submit(3)
        XCTAssertEqual(store.take(), 2, "the oldest kept")
        store.submit(4)
        store.putBack(2)
        XCTAssertEqual(store.takeAll(), [3, 4], "two newer win over a returned frame")
    }

    func testGlitchesCountStepsAgainstTheSource() {
        let r = 8.333
        let g = PresentDebugStats.glitches(
            steps: [(8.33, 8.33), (16.67, 8.33), (8.33, 16.67), (16.67, 16.67)], refreshMs: r)
        XCTAssertEqual(g.repeats, 1)
        XCTAssertEqual(g.skips, 1)
    }
}

/// The render thread's per-drawable glue: the phase pick feeding the pairing, the carry's
/// bound from the host's measured rate, and the debug window line.
final class DeadlineSelectorTests: XCTestCase {
    private let period = 0.008
    private let vendNs: Int64 = 1_000_000_000_000

    /// A selector whose phase settled on arrivals `fraction` of a period after the vend, ±0.005.
    private func settled(at fraction: Double, latePick: Bool = true) -> DeadlineSelector {
        var selector = DeadlineSelector(latePick: latePick, now: 0)
        for i in 0..<PhaseTracker.capacity {
            let f = fraction + (i % 2 == 0 ? 0.005 : -0.005)
            let decodedNs = vendNs + Int64((f * period * 1e9).rounded())
            selector.noteShown(decodedNs: decodedNs, period: period) { self.vendNs }
        }
        return selector
    }

    /// A drawable vended at 1.000 whose last safe pick is 1.0065, paired at `now`.
    private func pairing(
        _ selector: inout DeadlineSelector, now: Double = 1.001, drain: Bool = false,
        period: Double? = nil
    ) -> DeadlineSelector.Pairing {
        selector.pair(
            vendAt: 1.000, period: period ?? self.period, last: 1.0065, now: now, drain: drain)
    }

    private func assertPresent(
        _ p: DeadlineSelector.Pairing, at: Double, switched: Bool, _ note: String = "",
        file: StaticString = #filePath, line: UInt = #line
    ) {
        guard case .present(let got, let didSwitch) = p else {
            return XCTFail("\(note): expected a present, got \(p)", file: file, line: line)
        }
        XCTAssertEqual(got, at, accuracy: 1e-6, note, file: file, line: line)
        XCTAssertEqual(didSwitch, switched, note, file: file, line: line)
    }

    func testSmoothnessPresentsOnPairingWhateverThePhase() {
        var selector = settled(at: 0.7, latePick: false)
        assertPresent(pairing(&selector), at: 1.001, switched: false)
    }

    func testLatencyHoldsToTheCutoffUntilThePhaseIsKnown() {
        var fresh = DeadlineSelector(latePick: true, now: 0)
        assertPresent(pairing(&fresh), at: 1.0065, switched: false, "no phase yet")
        var unmeasured = settled(at: 0.3)
        assertPresent(
            pairing(&unmeasured, period: 0), at: 1.0065, switched: false, "no link period yet")
    }

    func testASettledPhasePicksAfterTheArrivalInTheSameRefresh() {
        var selector = settled(at: 0.3)
        assertPresent(pairing(&selector), at: 1.0064, switched: false)
    }

    func testAnArrivalNearTheWindowEndSwitchesToTheNextRefreshOnce() {
        var selector = settled(at: 0.7)
        assertPresent(pairing(&selector), at: 1.0016, switched: true, "the mapping flips")
        assertPresent(pairing(&selector), at: 1.0016, switched: false, "and stays flipped")
    }

    /// The pick runs, and commits a mapping switch, before the pairing decides to skip. The
    /// switch waits for the next present, which must not carry either; one present takes it.
    func testASwitchSurvivesASkippedPairing() {
        var selector = settled(at: 0.7)
        XCTAssertEqual(pairing(&selector, drain: true), .skip)
        assertPresent(pairing(&selector), at: 1.0016, switched: true, "the skipped switch")
        assertPresent(pairing(&selector), at: 1.0016, switched: false, "taken once")
    }

    func testPastTheCutoffSkipsAndNoWindowPresentsAtOnce() {
        var selector = DeadlineSelector(latePick: true, now: 0)
        XCTAssertEqual(pairing(&selector, now: 1.007), .skip, "would land a refresh late")
        XCTAssertEqual(pairing(&selector, drain: true), .skip)
        let noWindow = selector.pair(
            vendAt: 1.000, period: period, last: 0.999, now: 1.002, drain: false)
        assertPresent(noWindow, at: 1.002, switched: false)
    }

    /// Uniform arrivals have no phase: σ past 0.25 of a period is noise, not a pick.
    func testANoisyPhaseFallsBackToTheCutoff() {
        var selector = DeadlineSelector(latePick: true, now: 0)
        for i in 0..<PhaseTracker.capacity {
            let decodedNs = vendNs + Int64(Double(i) / 240 * period * 1e9)
            selector.noteShown(decodedNs: decodedNs, period: period) { self.vendNs }
        }
        assertPresent(pairing(&selector), at: 1.0065, switched: false)
    }

    func testNoShownSampleWithoutAPeriodOrADecodeStamp() {
        var selector = DeadlineSelector(latePick: true, now: 0)
        for _ in 0..<PhaseTracker.capacity {
            selector.noteShown(decodedNs: 5, period: 0) { XCTFail("asked for the vend"); return 0 }
            selector.noteShown(decodedNs: 0, period: period) { XCTFail("asked"); return 0 }
        }
        assertPresent(pairing(&selector), at: 1.0065, switched: false, "still no phase")
    }

    func testTheHostRateSetsTheCarryBoundOnceEveryWindow() {
        let p = 1.0 / 120
        var selector = DeadlineSelector(latePick: true, now: 10)
        selector.noteRate(at: 11.9, period: p) { XCTFail("the window is not up"); return 0 }
        selector.noteRate(at: 12, period: 0) { XCTFail("no period yet"); return 0 }
        XCTAssertTrue(
            selector.endWindow(report: true) { "x" }!.contains("carryBound=24 hostHz=0.0 "),
            "unmeasured: the base bound")
        // 238 frames in 2 s: 119 fps under a 120 Hz panel, so a hold ends within a beat.
        selector.noteRate(at: 12, period: p) { 238 }
        let bound = Carry.run(forSlowBy: 1 / p - 119, period: p)
        XCTAssertGreaterThan(bound, Carry.maxRun)
        XCTAssertTrue(
            selector.endWindow(report: true) { "x" }!.contains("carryBound=\(bound) hostHz=119.0 "))
        selector.noteRate(at: 13, period: p) { XCTFail("a new window opened at 12"); return 0 }
        // A host at the panel's rate keeps the base bound.
        selector.noteRate(at: 14, period: p) { 238 + 240 }
        XCTAssertTrue(
            selector.endWindow(report: true) { "x" }!.contains("carryBound=24 hostHz=120.0 "))
    }

    func testTheCarryHoldsOneFrameUnlessThePhaseSwitched() {
        var selector = DeadlineSelector(latePick: true, now: 0)
        XCTAssertTrue(selector.choose([1, 2], switched: false) == (1, 2))
        XCTAssertTrue(selector.choose([3, 4], switched: true) == (4, nil))
    }

    func testTheWindowLineNamesTheCountersThenResetsThem() {
        var selector = DeadlineSelector(latePick: true, now: 0)
        XCTAssertEqual(
            selector.endWindow(report: true) { "direct" },
            " carried=0 carryDrains=0 carryBound=24 hostHz=0.0 phaseSwitch=0 phaseRejected=0 "
                + "sigmaMs=-1.00 mapping=same comp=direct")
        _ = selector.choose([1, 2], switched: false)
        XCTAssertNil(selector.endWindow(report: false) { XCTFail("asked"); return "" })
        XCTAssertTrue(
            selector.endWindow(report: true) { "x" }!.hasPrefix(" carried=0 "),
            "an unreported window still resets")
    }

    func testTheWindowLineReportsTheSwitchAndTheSigma() {
        var selector = settled(at: 0.7)
        _ = pairing(&selector)
        let line = selector.endWindow(report: true) { "x" }!
        XCTAssertTrue(line.contains("phaseSwitch=1 phaseRejected=0 sigmaMs=0.02 mapping=next"))
    }

    // MARK: - holdForPick (the render thread's wait and carry)

    private func frame(_ pts: UInt64) -> ReadyFrame {
        var buffer: CVPixelBuffer?
        CVPixelBufferCreate(nil, 2, 2, kCVPixelFormatType_32BGRA, nil, &buffer)
        return ReadyFrame(
            ptsNs: pts, receivedNs: 0, decodedNs: 0, image: .video(buffer!, isHDR: false),
            flags: 0)
    }

    private func hold(
        _ first: UInt64, newer: [UInt64], switched: Bool = false, ahead: Double = -1,
        pacer: DeadlinePacer = DeadlinePacer()
    ) -> (shown: UInt64, kept: [UInt64], held: Bool, carried: Bool) {
        let ring = FrameStore<ReadyFrame>(policy: .newestTwo)
        for pts in newer { ring.submit(frame(pts)) }
        var selector = DeadlineSelector(latePick: true, now: 0)
        let choice = Stage2Pipeline.holdForPick(
            frame(first), at: CACurrentMediaTime() + ahead, switched: switched, period: period,
            selector: &selector, ring: ring, pacer: pacer)
        return (choice.show.ptsNs, ring.takeAll().map(\.ptsNs), choice.held, choice.carried)
    }

    func testABurstShowsTheOlderFrameAndReturnsTheNewerToTheRing() {
        let burst = hold(1, newer: [2])
        XCTAssertEqual(burst.shown, 1)
        XCTAssertEqual(burst.kept, [2])
        XCTAssertTrue(burst.carried)
        let alone = hold(1, newer: [])
        XCTAssertEqual(alone.shown, 1)
        XCTAssertEqual(alone.kept, [])
        XCTAssertFalse(alone.carried)
    }

    func testAPhaseSwitchShowsTheNewestAndKeepsNothing() {
        let switched = hold(1, newer: [2], switched: true)
        XCTAssertEqual(switched.shown, 2)
        XCTAssertEqual(switched.kept, [])
    }

    /// The pacer hears of a hold only when the thread really waited for the pick.
    func testOnlyAWaitedPickCountsAsHeld() {
        let idle = DeadlinePacer()
        XCTAssertFalse(hold(1, newer: [2], pacer: idle).held)
        XCTAssertTrue(idle.windowLine().text.contains("held=0 superseded=0"))
        let waited = DeadlinePacer()
        XCTAssertTrue(hold(1, newer: [2], ahead: 0.02, pacer: waited).held)
        XCTAssertTrue(waited.windowLine().text.contains("held=1 superseded=1"))
    }

    func testTheLatchGateOpensOnlyWhileTheVendsAreFed() {
        var gate = LatchGate()
        XCTAssertFalse(gate.open, "no history")
        for _ in 0..<8 { gate.note(shown: true) }
        XCTAssertTrue(gate.open, "every vend shown")
        for _ in 0..<4 { gate.note(shown: false); gate.note(shown: true) }
        XCTAssertFalse(gate.open, "60 fps content on a 120 Hz link")
        for _ in 0..<6 { gate.note(shown: true) }
        XCTAssertTrue(gate.open, "six of eight")
        gate.note(shown: false); gate.note(shown: false); gate.note(shown: false)
        XCTAssertFalse(gate.open, "three empty vends close it")
    }
}
#endif
