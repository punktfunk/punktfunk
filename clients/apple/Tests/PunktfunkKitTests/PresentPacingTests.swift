import XCTest

#if canImport(Metal)
import QuartzCore
@testable import PunktfunkKit

/// Present pacing: the stage-3 gate, stage-4 drawable handoff, decoded-video path, presenter
/// resolution, and per-platform defaults. The environment-only choices remain available for
/// on-device comparisons without becoming user settings.
final class PresentPacingTests: XCTestCase {
    // MARK: - PresentGate

    /// The depth-1 invariant: one present in flight. A second acquire while pending must fail
    /// (the frame stays in the ring for the presented handler's re-signal); release reopens.
    func testGateAdmitsOneInFlightPresent() {
        let gate = PresentGate()
        XCTAssertTrue(gate.tryAcquire(now: 0), "an idle gate must admit the first present")
        XCTAssertFalse(gate.tryAcquire(now: 0.001), "a pending present must close the gate")
        gate.release()
        XCTAssertTrue(gate.tryAcquire(now: 0.002), "release must reopen the gate")
        XCTAssertEqual(gate.drainForced(), 0, "no stale present was force-cleared")
    }

    /// Depth 2 (the PUNKTFUNK_GATE_DEPTH ladder rung — no longer a default; see
    /// `SessionPresenter.gateDepth`'s standing-queue post-mortem): a second present may queue
    /// behind the flip scanning out — the bound only bites at the THIRD. One release (a glass
    /// callback) reopens exactly one slot.
    func testGateDepthTwoAdmitsTwoInFlightPresents() {
        let gate = PresentGate(capacity: 2)
        XCTAssertTrue(gate.tryAcquire(now: 0))
        XCTAssertTrue(gate.tryAcquire(now: 0.001), "depth 2 must admit a queued second flip")
        XCTAssertFalse(gate.tryAcquire(now: 0.002), "the third present must wait for glass")
        gate.release()
        XCTAssertTrue(gate.tryAcquire(now: 0.003), "one glass callback frees one slot")
        XCTAssertFalse(gate.tryAcquire(now: 0.004))
        XCTAssertEqual(gate.drainForced(), 0)
    }

    /// Depth 2 staleness anchors to the OLDEST in-flight present: a full gate stays closed while
    /// the oldest is live, force-opens once it ages out, and the younger present keeps its slot.
    func testGateDepthTwoForceOpensOnTheOldestStalePresent() {
        let gate = PresentGate(capacity: 2)
        XCTAssertTrue(gate.tryAcquire(now: 10))
        XCTAssertTrue(gate.tryAcquire(now: 10.05))
        XCTAssertFalse(gate.tryAcquire(now: 10 + PresentGate.staleAfter - 0.01))
        XCTAssertTrue(gate.tryAcquire(now: 10 + PresentGate.staleAfter + 0.01))
        XCTAssertEqual(gate.drainForced(), 1)
        // The 10.05 present is still live, so the gate is full again right after the force-open.
        XCTAssertFalse(gate.tryAcquire(now: 10 + PresentGate.staleAfter + 0.02))
    }

    /// The lost-handler insurance: a present whose handler never fires (the macOS "presents
    /// aren't damage" hazard class) must not freeze the stream — past `staleAfter` the gate
    /// force-opens and counts the event for the PUNKTFUNK_PRESENT_DEBUG `forced` stat.
    func testGateForceOpensAfterStaleTimeout() {
        let gate = PresentGate()
        XCTAssertTrue(gate.tryAcquire(now: 10))
        // Within the stale window the gate stays closed.
        XCTAssertFalse(gate.tryAcquire(now: 10 + PresentGate.staleAfter - 0.01))
        // Past it, the pending present is presumed lost: reopen, and count the force-clear.
        XCTAssertTrue(gate.tryAcquire(now: 10 + PresentGate.staleAfter + 0.01))
        XCTAssertEqual(gate.drainForced(), 1)
        XCTAssertEqual(gate.drainForced(), 0, "drain resets the counter")
    }

    /// Release is idempotent (a late stale-path double-release must be harmless), and an
    /// acquire-then-release with no present (empty ring after arming) leaves the gate clean.
    func testGateReleaseIsIdempotent() {
        let gate = PresentGate()
        XCTAssertTrue(gate.tryAcquire(now: 0))
        gate.release()
        gate.release() // the stale-cleared present's handler firing late
        XCTAssertTrue(gate.tryAcquire(now: 0.001))
        XCTAssertEqual(gate.drainForced(), 0)
    }

    // MARK: - PresentPriority (the user-facing latency/smoothness intent)

    /// Resolution from the persisted settings: anything but an explicit "smooth" is latency
    /// (the default), and the buffer setting maps 0/out-of-range/garbage to Automatic (2).
    func testPresentPriorityResolution() {
        XCTAssertEqual(PresentPriority.resolve(setting: nil, bufferSetting: nil), .latency)
        XCTAssertEqual(PresentPriority.resolve(setting: "latency", bufferSetting: 3), .latency)
        XCTAssertEqual(PresentPriority.resolve(setting: "garbage", bufferSetting: nil), .latency)
        XCTAssertEqual(
            PresentPriority.resolve(setting: "smooth", bufferSetting: nil),
            .smooth(buffer: 2), "unset buffer = Automatic = 2")
        XCTAssertEqual(
            PresentPriority.resolve(setting: "smooth", bufferSetting: 0), .smooth(buffer: 2))
        XCTAssertEqual(
            PresentPriority.resolve(setting: "smooth", bufferSetting: 1), .smooth(buffer: 1))
        XCTAssertEqual(
            PresentPriority.resolve(setting: "smooth", bufferSetting: 3), .smooth(buffer: 3))
        XCTAssertEqual(
            PresentPriority.resolve(setting: "smooth", bufferSetting: 9),
            .smooth(buffer: 2), "out-of-range buffer = Automatic")
    }

    /// The intent→store mapping: latency runs the zero-queue newest-wins slot, smoothness the
    /// FIFO jitter buffer at the resolved capacity.
    func testPresentPriorityStorePolicy() {
        XCTAssertEqual(PresentPriority.latency.storePolicy, .newestWins)
        XCTAssertEqual(
            PresentPriority.smooth(buffer: 3).storePolicy, .fifo(capacity: 3))
    }

    // MARK: - FrameStore (the decoded-frame hand-off, both intents)

    /// Newest-wins (latency): submit replaces the undisplayed frame, take clears, putBack
    /// restores only into an empty slot — the exact pre-rebuild ReadyRing semantics.
    func testFrameStoreNewestWins() {
        let store = FrameStore<Int>(policy: .newestWins)
        XCTAssertNil(store.take())
        store.submit(1)
        store.submit(2)
        XCTAssertEqual(store.take(), 2, "the newer decode replaces the undisplayed frame")
        XCTAssertNil(store.take())
        store.putBack(7)
        store.submit(8) // a fresh decode beats the putBack
        store.putBack(7)
        XCTAssertEqual(store.take(), 8)
        XCTAssertEqual(store.drainSubmitted(), 3)
        let smoothing = store.drainSmoothing()
        XCTAssertEqual(smoothing.overflowDrops, 0)
        XCTAssertEqual(smoothing.underflows, 0)
    }

    /// FIFO (smoothness): take withholds frames until the buffer has PREROLLED to capacity —
    /// without preroll a steady stream drains on arrival and headroom never builds — then pops
    /// oldest-first.
    func testFrameStoreFifoPrerollsToCapacity() {
        let store = FrameStore<Int>(policy: .fifo(capacity: 2))
        store.submit(1)
        XCTAssertNil(store.take(), "one frame buffered — still building headroom")
        store.submit(2)
        XCTAssertEqual(store.take(), 1, "prerolled — pops the OLDEST")
        store.submit(3)
        XCTAssertEqual(store.take(), 2, "steady state: one in, oldest out")
        XCTAssertEqual(store.take(), 3)
    }

    /// FIFO overflow drops the OLDEST (bounded added latency, the newest keeps flowing) and
    /// counts it; running dry counts an underflow and re-arms preroll so headroom rebuilds.
    func testFrameStoreFifoOverflowAndUnderflow() {
        let store = FrameStore<Int>(policy: .fifo(capacity: 2))
        store.submit(1)
        store.submit(2)
        store.submit(3) // full — 1 (the oldest) goes
        XCTAssertEqual(store.take(), 2)
        XCTAssertEqual(store.take(), 3)
        XCTAssertNil(store.take(), "ran dry — an underflow, preroll re-arms")
        store.submit(4)
        XCTAssertNil(store.take(), "rebuilding headroom after the underflow")
        store.submit(5)
        XCTAssertEqual(store.take(), 4)
        let smoothing = store.drainSmoothing()
        XCTAssertEqual(smoothing.overflowDrops, 1)
        XCTAssertEqual(smoothing.underflows, 1)
    }

    /// FIFO putBack reinserts at the FRONT — a frame the render thread couldn't present is
    /// still the oldest, so present order is preserved.
    func testFrameStoreFifoPutBackPreservesOrder() {
        let store = FrameStore<Int>(policy: .fifo(capacity: 2))
        store.submit(1)
        store.submit(2)
        let f = store.take()
        XCTAssertEqual(f, 1)
        store.putBack(f!)
        XCTAssertEqual(store.take(), 1, "the returned frame stays first out")
        XCTAssertEqual(store.take(), 2)
    }

    // MARK: - LinkStallPolicy (stage-4's stale-link ladder)

    /// First stall relinks; a stall inside the window after that relink rebuilds (the field
    /// shape: one vend, then silence); a link that ran clean past the window relinks afresh.
    func testLinkStallPolicyRelinksOnceThenRebuilds() {
        var policy = LinkStallPolicy(rebuildWindow: 2)
        XCTAssertEqual(policy.onStall(now: 10), .relink, "the first stall gets a relink")
        XCTAssertEqual(policy.onStall(now: 10.3), .rebuild, "a stall right after it rebuilds")
        XCTAssertEqual(policy.onStall(now: 11.9), .rebuild, "still inside the window")
        XCTAssertEqual(policy.onStall(now: 12.5), .relink, "a clean run past the window resets")
        XCTAssertEqual(policy.onStall(now: 12.8), .rebuild)
    }

    // MARK: - LatestBox (stage-4's drawable hand-off)

    /// Newest-wins hand-off: `put` replaces (an unpresented older drawable returns to the
    /// layer's pool by release), `take` empties the slot.
    func testLatestBoxNewestWins() {
        let box = LatestBox<Int>()
        XCTAssertNil(box.take())
        box.put(1)
        box.put(2)
        XCTAssertEqual(box.take(), 2, "a fresher put replaces the unpresented value")
        XCTAssertNil(box.take(), "take empties the slot")
    }

    /// `putBack` fills only an EMPTY slot: the render thread returning a drawable it took but
    /// didn't present must never clobber a fresher one the link vended in between.
    func testLatestBoxPutBackNeverClobbersAFresherPut() {
        let box = LatestBox<Int>()
        box.put(1)
        let stale = box.take()
        XCTAssertEqual(stale, 1)
        box.putBack(stale!)
        XCTAssertEqual(box.take(), 1, "putBack into a still-empty slot restores the value")
        box.put(2)
        let taken = box.take()
        box.put(3) // the link vends a fresher drawable while the render thread holds `taken`
        box.putBack(taken!)
        XCTAssertEqual(box.take(), 3, "the fresher vend wins over the stale return")
    }

    // MARK: - PresenterChoice

    /// iOS defaults to the deadline link, tvOS to its decoded IOSurface video plane, and macOS to
    /// arrival-paced Metal. No selection or an unknown value falls back to the platform choice.
    func testPresenterChoiceFallsBackToPlatformDefault() {
        #if os(iOS)
        XCTAssertEqual(PresenterChoice.platformDefault, .stage4)
        #elseif os(tvOS)
        if #available(tvOS 17.4, *) {
            XCTAssertEqual(PresenterChoice.platformDefault, .decoded)
        } else {
            XCTAssertEqual(PresenterChoice.platformDefault, .stage4)
        }
        #else
        XCTAssertEqual(PresenterChoice.platformDefault, .stage2)
        #endif
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: nil, allowStage1: true),
            PresenterChoice.platformDefault)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "garbage", env: nil, allowStage1: true),
            PresenterChoice.platformDefault)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage2", env: nil, allowStage1: true), .stage2)
    }

    func testPresenterChoiceResolvesStage3FromSettingAndEnv() {
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage3", env: nil, allowStage1: true), .stage3)
        // The env override wins over the persisted setting (A/B without touching settings)…
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage2", env: "stage3", allowStage1: true), .stage3)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage3", env: "stage2", allowStage1: true), .stage2)
        // …but an EMPTY env var is "unset", not an override.
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage3", env: "", allowStage1: true), .stage3)
    }

    /// "stage4" (deadline pacing) resolves only on iOS/tvOS. On macOS — whose present path is
    /// entangled with the sync-off/DCP-panic saga — a synced-over "stage4" value maps back to
    /// the platform default instead of engaging an unvalidated pacing.
    func testPresenterChoiceGatesStage4ToVsyncLatchPlatforms() {
        #if os(macOS)
        XCTAssertNil(PresenterChoice.explicit(setting: "stage4", env: nil, allowStage1: true))
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage4", env: nil, allowStage1: true), .stage2)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: "stage4", allowStage1: true), .stage2)
        #else
        XCTAssertEqual(
            PresenterChoice.explicit(setting: "stage4", env: nil, allowStage1: true), .stage4)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: "stage4", allowStage1: true), .stage4)
        // The env override wins over the persisted setting, both directions.
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage4", env: "stage3", allowStage1: true), .stage3)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage2", env: "stage4", allowStage1: true), .stage4)
        #endif
    }

    /// The decoded video-layer path resolves only on tvOS. Other platforms treat an explicit
    /// value as unknown and retain their platform default.
    func testDecodedPresenterIsTvOSOnly() {
        #if os(tvOS)
        if #available(tvOS 17.4, *) {
            XCTAssertEqual(
                PresenterChoice.explicit(setting: nil, env: "decoded", allowStage1: true), .decoded)
        } else {
            XCTAssertNil(
                PresenterChoice.explicit(setting: nil, env: "decoded", allowStage1: true))
        }
        #else
        XCTAssertNil(
            PresenterChoice.explicit(setting: nil, env: "decoded", allowStage1: true))
        #endif
    }

    /// Stage-1 (the freeze-prone system-layer diagnostic) resolves only where allowed (DEBUG
    /// builds); a leftover "stage1" value in a release build maps back to the platform default.
    func testPresenterChoiceGatesStage1() {
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage1", env: nil, allowStage1: true), .stage1)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage1", env: nil, allowStage1: false),
            PresenterChoice.platformDefault)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: "stage1", allowStage1: false),
            PresenterChoice.platformDefault)
    }

    /// `explicit` is nil exactly when `resolve` would fall back to the platform default — the
    /// distinction the codec-conditional pacing default rides on.
    func testPresenterChoiceExplicitIsNilWithoutASelection() {
        XCTAssertNil(PresenterChoice.explicit(setting: nil, env: nil, allowStage1: true))
        XCTAssertNil(PresenterChoice.explicit(setting: "garbage", env: nil, allowStage1: true))
        XCTAssertNil(PresenterChoice.explicit(setting: "stage1", env: nil, allowStage1: false))
        XCTAssertEqual(
            PresenterChoice.explicit(setting: "stage2", env: nil, allowStage1: true), .stage2)
        XCTAssertEqual(
            PresenterChoice.explicit(setting: nil, env: "stage3", allowStage1: true), .stage3)
    }

    // MARK: - Session pacing (the macOS PyroWave swapID-panic mitigation)

    /// macOS PyroWave sessions under the DEFAULT stage-2 choice must get glass pacing (the
    /// one-in-flight gate is the "mismatched swapID's" kernel-panic mitigation); an EXPLICIT
    /// stage-2 pick must stay a faithful arrival-pacing A/B. Elsewhere the default is unchanged.
    func testPacingDefaultsPyroWaveToGlassOnMacOS() {
        #if os(macOS)
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage2, explicit: nil, codec: .pyrowave), .glass,
            "defaulted macOS PyroWave must serialize presents (swapID-panic mitigation)")
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage2, explicit: .stage2, codec: .pyrowave), .arrival,
            "an explicit stage-2 pick must keep arrival pacing (honest A/B)")
        #else
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage2, explicit: nil, codec: .pyrowave), .arrival)
        #endif
        // Non-PyroWave defaults keep arrival pacing under stage-2 everywhere.
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage2, explicit: nil, codec: .hevc), .arrival)
        // Stage-3 means glass regardless of codec or how it was chosen.
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage3, explicit: .stage3, codec: .hevc), .glass)
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage3, explicit: nil, codec: .pyrowave), .glass)
        // Stage-4 means deadline regardless of codec or how it was chosen.
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage4, explicit: nil, codec: .hevc), .deadline)
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage4, explicit: .stage4, codec: .pyrowave), .deadline)
        XCTAssertEqual(
            SessionPresenter.pacing(for: .decoded, explicit: .decoded, codec: .hevc), .decoded)
        XCTAssertEqual(
            SessionPresenter.pacing(for: .decoded, explicit: .decoded, codec: .pyrowave), .deadline)
    }

    func testDecodedPacingRequiresAVideoLayer() {
        XCTAssertNil(Stage2Pipeline(endToEndMeter: nil, pacing: .decoded))
    }

    func testSmoothnessKeepsTheBufferedPresenter() {
        XCTAssertEqual(
            SessionPresenter.effectivePacing(.decoded, priority: .latency), .decoded)
        XCTAssertEqual(
            SessionPresenter.effectivePacing(.decoded, priority: .smooth(buffer: 2)), .deadline)
        XCTAssertEqual(
            SessionPresenter.effectivePacing(
                .decoded, priority: .latency, videoLayerCompatible: false),
            .deadline)
        XCTAssertEqual(
            SessionPresenter.effectivePacing(.arrival, priority: .smooth(buffer: 2)), .arrival)
    }

    // MARK: - Windowed present mechanism (the macOS DCP swapID-panic mitigation picker)

    #if os(macOS)
    /// The safe-present setting: ON/unset → the validated transactional mitigation; an explicit
    /// OFF → the fast async path (the user accepted the affected-setup panic risk). The
    /// PUNKTFUNK_WINDOWED_PRESENT env lever overrides both ways, `surface` is env-only (the
    /// prototype mechanism), and garbage/empty env values are "unset", not an override.
    func testWindowedPresentModeResolution() {
        XCTAssertEqual(
            SessionPresenter.windowedPresentMode(setting: nil, env: nil), .transaction,
            "unset defaults to the panic mitigation")
        XCTAssertEqual(
            SessionPresenter.windowedPresentMode(setting: true, env: nil), .transaction)
        XCTAssertEqual(
            SessionPresenter.windowedPresentMode(setting: false, env: nil), .async,
            "an explicit opt-out gets the fast async path")
        // The dev env lever wins over the setting, both directions.
        XCTAssertEqual(
            SessionPresenter.windowedPresentMode(setting: true, env: "async"), .async)
        XCTAssertEqual(
            SessionPresenter.windowedPresentMode(setting: false, env: "transaction"),
            .transaction)
        XCTAssertEqual(
            SessionPresenter.windowedPresentMode(setting: true, env: "surface"), .surface,
            "the surface prototype is reachable via env only")
        XCTAssertEqual(
            SessionPresenter.windowedPresentMode(setting: false, env: "surface"), .surface)
        // Garbage/empty env = unset.
        XCTAssertEqual(
            SessionPresenter.windowedPresentMode(setting: nil, env: "garbage"), .transaction)
        XCTAssertEqual(
            SessionPresenter.windowedPresentMode(setting: false, env: ""), .async)
    }
    #endif

    // MARK: - macOS adaptive display

    #if os(macOS)
    func testAdaptiveSlotPacingResolution() {
        XCTAssertTrue(SessionPresenter.adaptiveSlotPaced(
            adaptiveSync: true, priority: .latency, pacing: .arrival))
        XCTAssertFalse(SessionPresenter.adaptiveSlotPaced(
            adaptiveSync: false, priority: .latency, pacing: .arrival))
        XCTAssertFalse(SessionPresenter.adaptiveSlotPaced(
            adaptiveSync: true, priority: .smooth(buffer: 2), pacing: .arrival))
        XCTAssertFalse(SessionPresenter.adaptiveSlotPaced(
            adaptiveSync: true, priority: .latency, pacing: .glass))
    }

    func testAdaptiveSlotRegimeUsesSparseImmediateAndDenseSlots() {
        var sparse = AdaptiveSlotRegime()
        XCTAssertTrue(sparse.update(ptsNs: 1_000_000_000))
        XCTAssertFalse(sparse.update(ptsNs: 1_028_571_429))
        XCTAssertFalse(sparse.update(ptsNs: 1_028_571_429), "a put-back is not a new sample")

        var dense = AdaptiveSlotRegime()
        XCTAssertTrue(dense.update(ptsNs: 1_000_000_000))
        XCTAssertTrue(dense.update(ptsNs: 1_016_666_667))

        var hitched = AdaptiveSlotRegime()
        XCTAssertTrue(hitched.update(ptsNs: 1_000_000_000))
        XCTAssertTrue(hitched.update(ptsNs: 1_008_333_333))
        XCTAssertTrue(hitched.update(ptsNs: 1_058_333_333), "one capped hitch keeps slots")

        var recoveryPts: UInt64 = 1_028_571_429
        for _ in 0..<4 {
            recoveryPts += 16_666_667
            _ = sparse.update(ptsNs: recoveryPts)
        }
        XCTAssertTrue(sparse.isSlotted, "sustained 60 fps returns to slots")
    }

    /// The macOS display-link hint: VRR-on asks for the stream rate down to a 24 Hz floor with
    /// callbacks capped at the stream rate; VRR-off pins the link at exactly the stream rate.
    func testMacFrameRateRangesAt60And120() {
        for hz: Float in [60, 120] {
            let vrr = SessionPresenter.frameRateRange(hz: hz, allowVRR: true)
            XCTAssertEqual(vrr.minimum, min(hz, 24))
            XCTAssertEqual(vrr.maximum, hz)
            XCTAssertEqual(vrr.preferred, hz)
            let fixed = SessionPresenter.frameRateRange(hz: hz, allowVRR: false)
            XCTAssertEqual(fixed.minimum, hz)
            XCTAssertEqual(fixed.maximum, hz)
            XCTAssertEqual(fixed.preferred, hz)
        }
    }
    #endif

    // MARK: - Glass-gate depth

    /// The in-flight present budget is 1 EVERYWHERE: any deeper gate keeps a standing queue —
    /// the 2026-07 iPad depth-2 experiment regressed display latency 14→22–28 ms (see
    /// `SessionPresenter.gateDepth`'s post-mortem). macOS additionally pins the env lever (glass
    /// there is the swapID-panic mitigation — strict serialization is its point);
    /// PUNKTFUNK_GATE_DEPTH still reproduces the standing-queue ladder on iOS/tvOS.
    /// Out-of-range/garbage values are ignored.
    func testGateDepthPlatformDefaultsAndEnvOverride() {
        #if os(macOS)
        XCTAssertEqual(SessionPresenter.gateDepth(env: nil), 1)
        XCTAssertEqual(SessionPresenter.gateDepth(env: "2"), 1, "macOS is pinned to 1")
        #else
        XCTAssertEqual(
            SessionPresenter.gateDepth(env: nil), 1,
            "any depth >1 is a standing queue — one refresh of display latency per slot")
        XCTAssertEqual(SessionPresenter.gateDepth(env: "2"), 2, "the on-device ladder lever")
        #endif
        XCTAssertEqual(
            SessionPresenter.gateDepth(env: "0"), SessionPresenter.gateDepth(env: nil),
            "out-of-range env values fall back to the platform depth")
        XCTAssertEqual(
            SessionPresenter.gateDepth(env: "garbage"), SessionPresenter.gateDepth(env: nil))
    }
}
#endif
