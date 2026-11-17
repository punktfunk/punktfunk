// The two decisions that stop a rebuild loop, driven with a synthetic clock.
//
// The loop's shape, for the plant-the-defect cases below: the voice-processing engine fails to
// start (~1.9 s spent trying), the fallback comes up, and its own HAL fallout retriggers the
// recovery ~0.6 s later — forever. Restore either defect (retry the failed topology, or keep the
// flat 0.5 s floor) and the session pays an audio gap every ~2.5 s for as long as it lives.

import XCTest

@testable import PunktfunkKit

final class AudioRebuildPolicyTests: XCTestCase {
    // MARK: - RebuildBackoff

    /// The first trigger of a session waits only the burst-coalescing debounce.
    func testFirstTriggerWaitsOnlyTheDebounce() {
        var backoff = RebuildBackoff()
        XCTAssertEqual(backoff.delay(now: 1000), RebuildBackoff.debounce)
    }

    /// One rebuild, then quiet: the next real device switch minutes later is answered at full
    /// responsiveness — the ladder must never make a HEALTHY recovery sluggish.
    func testAnIsolatedSwitchLongAfterTheLastRebuildResetsTheChain() {
        var backoff = RebuildBackoff()
        _ = backoff.delay(now: 1000)
        backoff.noteRebuild(at: 1000.2)
        // Chained once (a second switch soon after — legitimate, e.g. AirPods out then back in).
        _ = backoff.delay(now: 1001)
        backoff.noteRebuild(at: 1002)
        // Minutes of quiet, then a fresh switch: base debounce again, chain forgotten.
        XCTAssertEqual(backoff.delay(now: 1300), RebuildBackoff.debounce)
        XCTAssertEqual(backoff.chain, 0)
    }

    /// The looping recovery, against the real constants: a trigger 0.6 s after every rebuild, ten
    /// minutes long. The flat 0.5 s floor produced a rebuild every ~2.5 s — ~240 audio gaps.
    /// The ladder must cut that by an order of magnitude and settle at the floor cap.
    func testAChainedLoopBacksOffToTheFloorCap() {
        var backoff = RebuildBackoff()
        var now: TimeInterval = 0
        var rebuilds = 0
        var lastDelay: TimeInterval = 0
        let end: TimeInterval = 600
        while now < end {
            lastDelay = backoff.delay(now: now)
            now += lastDelay // the scheduled rebuild fires...
            backoff.noteRebuild(at: now)
            rebuilds += 1
            now += 0.6 // ...and its fallout retriggers the recovery 0.6 s later.
        }
        XCTAssertEqual(
            lastDelay, RebuildBackoff.floorCap - 0.6, accuracy: 0.01,
            "a persistent loop should settle at one rebuild per floorCap")
        XCTAssertLessThanOrEqual(
            rebuilds, 30,
            "\(rebuilds) rebuilds in 10 min — the ladder is not escalating (the shipped flat "
                + "floor produced ~240)")
        // And the loop's END must restore responsiveness: quiet, then a real switch.
        XCTAssertEqual(backoff.delay(now: now + 120), RebuildBackoff.debounce)
    }

    /// The ladder's exponent is clamped — a loop that runs for hours must neither overflow nor
    /// push the interval past the cap.
    func testTheFloorNeverExceedsTheCap() {
        var backoff = RebuildBackoff()
        var now: TimeInterval = 0
        for _ in 0..<1000 {
            let delay = backoff.delay(now: now)
            XCTAssertLessThanOrEqual(delay, RebuildBackoff.floorCap)
            now += delay
            backoff.noteRebuild(at: now)
            now += 0.1
        }
    }

    #if os(macOS)
    // MARK: - CombinedTopologyGate

    /// The loop's fuel: re-attempting the voice-processing start that just failed. Same input
    /// device ⇒ never again.
    func testAFailureLatchesForTheDeviceItFailedOn() {
        var gate = CombinedTopologyGate()
        XCTAssertTrue(gate.shouldTry(input: 42), "an unfailed gate must allow the attempt")
        gate.noteFailure(input: 42)
        XCTAssertFalse(gate.shouldTry(input: 42))
        XCTAssertFalse(gate.shouldTry(input: 42), "the latch must hold across rebuilds")
    }

    /// The failure is a property of the DEVICE: a different default input earns a fresh attempt,
    /// and its own failure latches again — one attempt per device change can never loop.
    func testADifferentInputDeviceEarnsOneFreshAttempt() {
        var gate = CombinedTopologyGate()
        gate.noteFailure(input: 42)
        XCTAssertTrue(gate.shouldTry(input: 7))
        gate.noteFailure(input: 7)
        XCTAssertFalse(gate.shouldTry(input: 7))
        // Back to the first device: the earlier failure may have been mid-transition — one fresh
        // attempt again, not a permanent ban.
        XCTAssertTrue(gate.shouldTry(input: 42))
    }

    /// "No resolvable input device" is a real failure key too, distinct from "never failed".
    func testFailingWithNoInputDeviceLatchesForNoInputDevice() {
        var gate = CombinedTopologyGate()
        gate.noteFailure(input: nil)
        XCTAssertFalse(gate.shouldTry(input: nil))
        XCTAssertTrue(gate.shouldTry(input: 42), "a device appearing is a device change")
    }
    #endif
}
