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
    /// restores only into an empty slot.
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
        XCTAssertEqual(store.submittedTotal(), 3)
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

    /// The owner's policy above it: one fresh layer, then hold while the display re-plumbs; a
    /// clean run past the window earns a fresh layer again.
    func testWedgePolicyRebuildsOnceThenHolds() {
        var policy = WedgePolicy(resetAfter: 30)
        XCTAssertEqual(policy.next(now: 100), .rebuild)
        XCTAssertEqual(policy.next(now: 103), .hold, "a rebuild during the wait starts it over")
        XCTAssertEqual(policy.next(now: 120), .hold)
        XCTAssertEqual(policy.wedges, 3)
        XCTAssertEqual(policy.next(now: 200), .rebuild, "a clean run past the window resets")
    }

    // MARK: - LinkWatchdog (the stale-link watchdog around that ladder)

    /// A decoded frame that finds no drawable judges the link only once a link runs and 250 ms
    /// passed since its last vend; a relink or a vend restarts that clock.
    func testWatchdogJudgesOnlyARunningLinkPastTheStaleWindow() {
        var dog = LinkWatchdog(now: 100)
        XCTAssertEqual(
            dog.noVend(linkRunning: false, canReport: true, now: 101), .none,
            "the first frame has not started a link yet")
        XCTAssertEqual(
            dog.noVend(linkRunning: true, canReport: true, now: 100.25), .none, "exactly 250 ms")
        dog.noteVend(at: 100.25)
        XCTAssertEqual(dog.noVend(linkRunning: true, canReport: true, now: 100.5), .none)
        XCTAssertEqual(
            dog.noVend(linkRunning: true, canReport: true, now: 100.625), .relink(stalledMs: 375))
        XCTAssertEqual(
            dog.noVend(linkRunning: true, canReport: true, now: 100.75), .none,
            "the relink restarted the clock")
        XCTAssertEqual(dog.lastVend, 100.625)
    }

    /// The field shape of a wedged layer: a relink, one vend, silence again within 2 s. The
    /// report fires once, then the watchdog holds until the system shows a frame.
    func testWatchdogReportsAWedgeOnceThenHoldsUntilGlass() {
        var dog = LinkWatchdog(now: 100)
        XCTAssertEqual(
            dog.noVend(linkRunning: true, canReport: true, now: 100.5), .relink(stalledMs: 500))
        XCTAssertEqual(
            dog.noVend(linkRunning: true, canReport: true, now: 101), .wedged(stalledMs: 500))
        XCTAssertTrue(dog.isWedged)
        XCTAssertEqual(
            dog.noVend(linkRunning: true, canReport: true, now: 200), .none,
            "the fresh link stays; the rebuild replaces this pipeline")
        var asked = 0
        XCTAssertFalse(dog.rearm(glass: { asked += 1; return false }), "a vend is not glass")
        XCTAssertTrue(dog.isWedged)
        XCTAssertTrue(dog.rearm(glass: { asked += 1; return true }))
        XCTAssertEqual(asked, 2)
        XCTAssertFalse(dog.isWedged)
        dog.noteVend(at: 201)
        XCTAssertEqual(
            dog.noVend(linkRunning: true, canReport: true, now: 201.5), .relink(stalledMs: 500),
            "re-armed with a fresh ladder")
    }

    /// Glass is asked for only while wedged, so a healthy watchdog never consumes it.
    func testWatchdogLeavesGlassAloneWhileHealthy() {
        var dog = LinkWatchdog(now: 100)
        var asked = 0
        XCTAssertFalse(dog.rearm(glass: { asked += 1; return true }))
        XCTAssertEqual(asked, 0)
    }

    /// With nobody listening for a wedge every stall just relinks, the ladder's `rebuild`
    /// included, and the watchdog never latches.
    func testWatchdogWithoutAListenerKeepsRelinking() {
        var dog = LinkWatchdog(now: 100)
        let stalls: [Double] = [100.5, 101, 101.5, 102.75]
        for now in stalls {
            guard case .relink = dog.noVend(linkRunning: true, canReport: false, now: now) else {
                return XCTFail("stall at \(now) must relink")
            }
        }
        XCTAssertFalse(dog.isWedged)
    }

    // MARK: - Deadline drawable count

    func testDrawableCountTakesTwoOrThreeAndIgnoresTheRest() {
        XCTAssertEqual(Stage2Pipeline.drawableCount(env: nil), 3)
        XCTAssertEqual(Stage2Pipeline.drawableCount(env: "2"), 2)
        XCTAssertEqual(Stage2Pipeline.drawableCount(env: "3"), 3)
        for env in ["1", "4", "-2", "two", ""] {
            XCTAssertEqual(Stage2Pipeline.drawableCount(env: env), 3, "\(env)")
        }
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

    /// The render thread drops a drawable past its pick: only that one, never a fresher vend.
    func testLatestBoxTakeWhereChecksTheValueItTakes() {
        let box = LatestBox<Int>()
        XCTAssertNil(box.peek { $0 })
        box.put(5)
        XCTAssertEqual(box.peek { $0 * 2 }, 10, "peek reads without taking")
        XCTAssertNil(box.take(where: { $0 < 5 }), "a fresher value stays")
        XCTAssertEqual(box.take(where: { $0 == 5 }), 5)
        XCTAssertNil(box.take(), "the matching take emptied the slot")
    }

    // MARK: - PresenterChoice

    /// iOS and tvOS default to the deadline link, macOS to arrival-paced Metal. No selection, an
    /// empty env var or an unknown value falls back to the platform choice.
    func testPresenterChoiceFallsBackToPlatformDefault() {
        #if os(iOS) || os(visionOS) || os(tvOS)
        XCTAssertEqual(PresenterChoice.platformDefault, .stage4)
        #else
        XCTAssertEqual(PresenterChoice.platformDefault, .stage2)
        #endif
        for env in [nil, "", "garbage"] {
            XCTAssertNil(PresenterChoice.explicit(env: env), "\(String(describing: env))")
            XCTAssertEqual(PresenterChoice.resolve(env: env), PresenterChoice.platformDefault)
        }
    }

    func testPresenterChoiceResolvesTheStagesFromEnv() {
        XCTAssertEqual(PresenterChoice.resolve(env: "stage1"), .stage1)
        XCTAssertEqual(PresenterChoice.resolve(env: "stage2"), .stage2)
        XCTAssertEqual(PresenterChoice.resolve(env: "stage3"), .stage3)
    }

    /// "stage4" (deadline pacing) resolves only off macOS, whose present path has separate
    /// synchronization constraints; there it maps back to the platform default.
    func testPresenterChoiceGatesStage4ToVsyncLatchPlatforms() {
        #if os(macOS)
        XCTAssertNil(PresenterChoice.explicit(env: "stage4"))
        XCTAssertEqual(PresenterChoice.resolve(env: "stage4"), .stage2)
        #else
        XCTAssertEqual(PresenterChoice.resolve(env: "stage4"), .stage4)
        #endif
    }

    /// The decoded video-layer A/B resolves on iOS and tvOS 17.4+. Elsewhere an explicit value is
    /// unknown and the platform default stays.
    func testDecodedPresenterIsIOSAndTvOSOnly() {
        #if os(tvOS) || os(iOS)
        if #available(iOS 17.4, tvOS 17.4, *) {
            XCTAssertEqual(PresenterChoice.explicit(env: "decoded"), .decoded)
        } else {
            XCTAssertNil(PresenterChoice.explicit(env: "decoded"))
        }
        #else
        XCTAssertNil(PresenterChoice.explicit(env: "decoded"))
        #endif
    }

    // MARK: - Session pacing

    /// Stage-2 is arrival pacing for every codec, PyroWave included: the glass gate cost about a
    /// refresh per frame and never prevented the DCP panic. Explicit stages keep their pacing.
    func testPacingMapsStagesTheSameForEveryCodec() {
        XCTAssertEqual(SessionPresenter.pacing(for: .stage2, codec: .pyrowave), .arrival)
        XCTAssertEqual(SessionPresenter.pacing(for: .stage2, codec: .hevc), .arrival)
        XCTAssertEqual(SessionPresenter.pacing(for: .stage3, codec: .hevc), .glass)
        XCTAssertEqual(SessionPresenter.pacing(for: .stage3, codec: .pyrowave), .glass)
        XCTAssertEqual(SessionPresenter.pacing(for: .stage4, codec: .hevc), .deadline)
        XCTAssertEqual(SessionPresenter.pacing(for: .stage4, codec: .pyrowave), .deadline)
        XCTAssertEqual(SessionPresenter.pacing(for: .decoded, codec: .hevc), .decoded)
        XCTAssertEqual(SessionPresenter.pacing(for: .decoded, codec: .pyrowave), .deadline)
    }

    @MainActor
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

    // MARK: - Present policy

    /// V-Sync schedules on the grid, adaptive slots need the latency path with V-Sync off, the
    /// smoothness store takes one frame per slot, and the env knob overrides each for A/B.
    func testPresentPolicyResolution() {
        func policy(
            _ env: String?, vsync: Bool = false, vsyncPaced: Bool = false,
            adaptive: Bool = false
        ) -> PresentPolicy {
            PresentPolicy.resolve(
                env: env, vsync: vsync, vsyncPaced: vsyncPaced, adaptiveSlotPaced: adaptive)
        }
        let adaptive = policy(nil, adaptive: true)
        XCTAssertEqual(
            adaptive,
            PresentPolicy(adaptiveSlot: true, fixedSlot: false, fixedVsync: false, directGate: true))
        XCTAssertEqual(adaptive.label(.arrival), "direct")
        XCTAssertEqual(policy("garbage", adaptive: true), adaptive, "an unknown mode is no mode")
        // `adaptive` keeps the slot regime and drops the Direct gate, for A/B.
        let slotOnly = policy("adaptive", adaptive: true)
        XCTAssertEqual(slotOnly, PresentPolicy(adaptiveSlot: true, fixedSlot: false, fixedVsync: false))
        XCTAssertEqual(slotOnly.label(.arrival), "adaptive")
        XCTAssertFalse(policy("adaptive").directGate, "no gate off an adaptive panel")

        let vsync = policy(nil, vsync: true, adaptive: true)
        XCTAssertEqual(vsync, PresentPolicy(adaptiveSlot: false, fixedSlot: false, fixedVsync: true))
        XCTAssertEqual(vsync.label(.arrival), "vsync")

        let smooth = policy(nil, vsyncPaced: true)
        XCTAssertEqual(smooth, PresentPolicy(adaptiveSlot: false, fixedSlot: true, fixedVsync: false))
        XCTAssertEqual(smooth.label(.arrival), "slot")

        XCTAssertEqual(
            policy("slot", adaptive: true),
            PresentPolicy(adaptiveSlot: false, fixedSlot: true, fixedVsync: false))
        let immediate = policy("immediate", vsync: true, adaptive: true)
        XCTAssertEqual(
            immediate, PresentPolicy(adaptiveSlot: false, fixedSlot: false, fixedVsync: false))
        XCTAssertEqual(immediate.label(.arrival), "immediate")
        XCTAssertEqual(
            policy("vsync", adaptive: true),
            PresentPolicy(adaptiveSlot: false, fixedSlot: false, fixedVsync: true))

        // The other pacings name themselves; the display-link policy doesn't apply there.
        XCTAssertEqual(adaptive.label(.glass), "glass")
        XCTAssertEqual(adaptive.label(.deadline), "deadline")
        XCTAssertEqual(adaptive.label(.decoded), "decoded")
    }

    // MARK: - ArrivalSlots (the arrival/glass render thread's slot decisions)

    private static let immediate = PresentPolicy(
        adaptiveSlot: false, fixedSlot: false, fixedVsync: false)
    private static let vsync = PresentPolicy(
        adaptiveSlot: false, fixedSlot: false, fixedVsync: true)
    private static let slot = PresentPolicy(
        adaptiveSlot: false, fixedSlot: true, fixedVsync: false)
    private static let adaptive = PresentPolicy(
        adaptiveSlot: true, fixedSlot: false, fixedVsync: false, directGate: true)

    /// A display-link grid that answers `answer` and records what it was asked.
    private final class Grid {
        var answer: CFTimeInterval?
        private(set) var asked: [CFTimeInterval] = []
        init(_ answer: CFTimeInterval?) { self.answer = answer }
        func next(_ t: CFTimeInterval) -> CFTimeInterval? {
            asked.append(t)
            return answer
        }
    }

    private let hz120 = 1.0 / 120

    func testRefreshFallsBackToTheFastestPanelWithoutAHint() {
        XCTAssertEqual(ArrivalSlots.refresh(panelInterval: 0), 1.0 / 120)
        XCTAssertEqual(ArrivalSlots.refresh(panelInterval: -1), 1.0 / 120)
        XCTAssertEqual(ArrivalSlots.refresh(panelInterval: 1.0 / 60), 1.0 / 60)
    }

    /// Only the fixed-slot policy asks the grid before the take; every other policy decides
    /// after it, so its wake is never gated here.
    func testFixedSlotAsksTheGridOnlyUnderTheFixedSlotPolicy() {
        for policy in [Self.immediate, Self.vsync, Self.adaptive] {
            let grid = Grid(100)
            let slots = ArrivalSlots(policy: policy)
            let fixed = slots.fixedSlot(now: 99.99, nextVsync: grid.next)
            XCTAssertNil(fixed.target)
            XCTAssertFalse(fixed.taken)
            XCTAssertEqual(grid.asked, [], "\(policy)")
        }
        let grid = Grid(100)
        let fixed = ArrivalSlots(policy: Self.slot).fixedSlot(now: 99.99, nextVsync: grid.next)
        XCTAssertEqual(fixed.target, 100)
        XCTAssertFalse(fixed.taken)
        XCTAssertEqual(grid.asked, [99.99])
    }

    /// A stale grid has no target, so it books nothing and gates nothing.
    func testFixedSlotWithAStaleGridNeitherGatesNorBooks() {
        var slots = ArrivalSlots(policy: Self.slot)
        let fixed = slots.fixedSlot(now: 99.99, nextVsync: Grid(nil).next)
        XCTAssertNil(fixed.target)
        XCTAssertFalse(fixed.taken)
        let plan = slots.plan(
            ptsNs: 1, dueMediaTime: nil, now: 99.99, fixedTarget: nil, direct: false,
            refresh: hz120, nextVsync: Grid(nil).next)
        XCTAssertEqual(plan, .present(at: nil, slot: nil, holdUnit: hz120))
        slots.presented(slot: nil)
        XCTAssertEqual(slots.lastPresentTarget, 0, "no target, nothing booked")
    }

    /// The slot a present booked gates a wake that targets it again, to within 2 ms.
    func testFixedSlotGatesTheBookedTargetOnly() {
        var slots = ArrivalSlots(policy: Self.slot)
        slots.presented(slot: 100)
        let rows: [(target: Double, taken: Bool)] = [
            (100, true), (100.0019, true), (99.9981, true), (100.0021, false), (99.9979, false),
            (100.0083, false),
        ]
        for row in rows {
            let fixed = slots.fixedSlot(now: 99.99, nextVsync: Grid(row.target).next)
            XCTAssertEqual(fixed.taken, row.taken, "target \(row.target)")
            XCTAssertEqual(fixed.target, row.target)
        }
    }

    /// Fixed-slot presents book the link target; fixed-V-Sync and immediate presents book none.
    /// The grid is asked for the first vsync at or after the later of now and the frame's due
    /// time, only when the policy schedules on it.
    func testPlanSchedulesOnTheGridOnlyWhereThePolicyDoes() {
        struct Row {
            var name: String
            var policy: PresentPolicy
            var fixed: Double?
            var due: Double?
            var grid: Double?
            var want: ArrivalSlots.Plan
            var asked: [Double]
        }
        let none = ArrivalSlots.Plan.present(at: nil, slot: nil, holdUnit: hz120)
        let rows = [
            Row(name: "immediate", policy: Self.immediate, grid: 50, want: none, asked: []),
            Row(
                name: "vsync", policy: Self.vsync, grid: 50,
                want: .present(at: 50, slot: nil, holdUnit: hz120), asked: [49.9]),
            Row(
                name: "vsync, due ahead", policy: Self.vsync, due: 50.2, grid: 50.3,
                want: .present(at: 50.3, slot: nil, holdUnit: hz120), asked: [50.2]),
            Row(
                name: "vsync, due passed", policy: Self.vsync, due: 40, grid: 50,
                want: .present(at: 50, slot: nil, holdUnit: hz120), asked: [49.9]),
            Row(name: "vsync, stale grid", policy: Self.vsync, want: none, asked: [49.9]),
            Row(
                name: "slot, its target", policy: Self.slot, fixed: 50, due: 50.2, grid: 99,
                want: .present(at: 50, slot: 50, holdUnit: hz120), asked: []),
            Row(
                name: "slot, stale grid at the take", policy: Self.slot, grid: 50,
                want: .present(at: 50, slot: nil, holdUnit: hz120), asked: [49.9]),
            Row(
                name: "slot, stale grid, due ahead", policy: Self.slot, due: 50.2,
                want: none, asked: [50.2]),
        ]
        for row in rows {
            var slots = ArrivalSlots(policy: row.policy)
            let grid = Grid(row.grid)
            let plan = slots.plan(
                ptsNs: 1, dueMediaTime: row.due, now: 49.9, fixedTarget: row.fixed, direct: false,
                refresh: hz120, nextVsync: grid.next)
            XCTAssertEqual(plan, row.want, row.name)
            XCTAssertEqual(grid.asked, row.asked, row.name)
        }
    }

    /// Dense content books one present per link slot; the second frame of a slot goes back.
    func testAdaptivePlanBooksOneFrameForEachSlot() {
        var slots = ArrivalSlots(policy: Self.adaptive)
        let step: UInt64 = 8_333_333
        var pts: UInt64 = 1_000_000_000
        func plan(slot: Double) -> ArrivalSlots.Plan {
            pts += step
            return slots.plan(
                ptsNs: pts, dueMediaTime: nil, now: 49.9, fixedTarget: nil, direct: false,
                refresh: hz120, nextVsync: Grid(slot).next)
        }
        guard case .present(let at, let booked, _) = plan(slot: 50) else {
            return XCTFail("the first frame of a slot presents")
        }
        XCTAssertEqual(at, 50)
        XCTAssertEqual(booked, 50)
        slots.presented(slot: booked)
        XCTAssertEqual(plan(slot: 50.001), .putBack, "the slot is taken")
        XCTAssertEqual(slots.lastPresentTarget, 50, "a put-back books nothing")
        XCTAssertEqual(plan(slot: 50.0083), .present(at: 50.0083, slot: 50.0083, holdUnit: hz120))
    }

    /// While the layer scans out Direct the slot is bypassed, but the regime keeps learning the
    /// content interval so the Direct gate judges holds against it.
    func testDirectPresentsAtOnceAndKeepsLearningTheInterval() {
        var slots = ArrivalSlots(policy: Self.adaptive)
        let grid = Grid(50)
        var pts: UInt64 = 1_000_000_000
        var last: ArrivalSlots.Plan?
        for _ in 0..<2 {
            last = slots.plan(
                ptsNs: pts, dueMediaTime: nil, now: 49.9, fixedTarget: nil, direct: true,
                refresh: hz120, nextVsync: grid.next)
            pts += 33_000_000
        }
        XCTAssertEqual(last, .present(at: nil, slot: nil, holdUnit: 0.033), "an immediate present")
        XCTAssertEqual(grid.asked, [], "Direct never asks for a slot")
        // Back to the composited window: 33 ms content is sparse, so it still presents at once.
        let after = slots.plan(
            ptsNs: pts, dueMediaTime: nil, now: 49.9, fixedTarget: nil, direct: false,
            refresh: hz120, nextVsync: grid.next)
        XCTAssertEqual(after, .present(at: nil, slot: nil, holdUnit: 0.033))
        XCTAssertEqual(grid.asked, [])
    }

    /// Sparse input presents at once even off Direct; the hold unit never drops under the
    /// panel's refresh, and a policy without the regime never learns an interval.
    func testHoldUnitIsTheLongerOfRefreshAndContentInterval() {
        func hold(_ policy: PresentPolicy, gap: UInt64, refresh: Double) -> Double? {
            var slots = ArrivalSlots(policy: policy)
            var plan: ArrivalSlots.Plan?
            for i: UInt64 in 0..<3 {
                plan = slots.plan(
                    ptsNs: 1_000_000_000 + i * gap, dueMediaTime: nil, now: 49.9,
                    fixedTarget: nil, direct: true, refresh: refresh, nextVsync: Grid(50).next)
            }
            if case .present(_, _, let unit) = plan { return unit }
            return nil
        }
        let sparse = hold(Self.adaptive, gap: 33_000_000, refresh: hz120)
        XCTAssertEqual(sparse!, 0.033, accuracy: 1e-9)
        let dense = hold(Self.adaptive, gap: 4_000_000, refresh: 1.0 / 60)
        XCTAssertEqual(dense!, 1.0 / 60, accuracy: 1e-9)
        let fixed = hold(Self.slot, gap: 33_000_000, refresh: hz120)
        XCTAssertEqual(fixed!, hz120, accuracy: 1e-9)
    }

    /// A frame put back and taken again carries the same pts: the regime must not count it.
    func testARetakenFrameDoesNotMoveTheRegime() {
        var slots = ArrivalSlots(policy: Self.adaptive)
        for _ in 0..<3 {
            let plan = slots.plan(
                ptsNs: 1_000_000_000, dueMediaTime: nil, now: 49.9, fixedTarget: nil,
                direct: true, refresh: hz120, nextVsync: Grid(50).next)
            XCTAssertEqual(plan, .present(at: nil, slot: nil, holdUnit: hz120), "no interval yet")
        }
    }

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

    // MARK: - pf-present glass metrics

    /// Fixed 240 Hz: intervals are multiples of the refresh. Adaptive 24–120 Hz with an 8.33 ms
    /// step: 1 = the fastest refresh, 3 = 25 ms, 4 = 33 ms — the 35 fps alternation.
    func testPanelGridUnits() {
        let fixed = PanelInfo(minHz: 240, maxHz: 240)
        XCTAssertFalse(fixed.isAdaptive)
        XCTAssertEqual(fixed.gridUnits(interval: 1 / 240), 1)
        XCTAssertEqual(fixed.gridUnits(interval: 2 / 240 + 0.0005), 2)
        let adaptive = PanelInfo(minHz: 24, maxHz: 120, granularity: 1 / 120)
        XCTAssertTrue(adaptive.isAdaptive)
        XCTAssertEqual(adaptive.gridUnits(interval: 1 / 120), 1)
        XCTAssertEqual(adaptive.gridUnits(interval: 0.025), 3)
        XCTAssertEqual(adaptive.gridUnits(interval: 0.0333), 4)
        XCTAssertEqual(PanelInfo(minHz: 0, maxHz: 0).gridUnits(interval: 0.02), 0)
    }

    /// 60 on 120: every interval two steps, judder 0. 35 on 120: a 3/4 alternation whose
    /// minority share is the judder number; `cadErr` then says whether it follows the source.
    func testGridHistogramNamesTheModeAndItsMinority() {
        let panel = PanelInfo(minHz: 24, maxHz: 120, granularity: 1 / 120)
        let even = PresentDebugStats.gridHistogram(
            deltasMs: Array(repeating: 16.7, count: 60), panel: panel)
        XCTAssertEqual(even.hist, "2:60")
        XCTAssertEqual(even.judder, 0, accuracy: 0.001)
        let alternating = PresentDebugStats.gridHistogram(
            deltasMs: [25, 33.3, 25, 33.3, 25, 25, 33.3, 25, 25, 25], panel: panel)
        XCTAssertEqual(alternating.hist, "3:7,4:3")
        XCTAssertEqual(alternating.judder, 0.3, accuracy: 0.001)
        XCTAssertEqual(PresentDebugStats.gridHistogram(deltasMs: [], panel: panel).hist, "")
    }

    /// Cadence error pairs consecutive on-glass frames against their source spacing; a repeat
    /// carries no source cadence, so it is counted but never paired, and a dropped present
    /// (no system stamp) leaves no glass sample at all.
    func testCadenceErrorPairsGlassWithSourceAndSkipsRepeats() {
        let stats = PresentDebugStats(cadence: nil, pace: { "test" }, linkPeriod: { 0 })
        let ms: Int64 = 1_000_000
        // Source 16.7 ms apart, glass 16.7 then 25 ms apart: errors 0 and 8.3.
        stats.presented(atNs: 100 * ms, issuedNs: 95 * ms, ptsNs: 1_000 * UInt64(ms), decodedNs: 98 * ms)
        stats.presented(
            atNs: 116_700_000, issuedNs: 110 * ms, ptsNs: 1_016_700_000, decodedNs: 112 * ms)
        stats.presented(
            atNs: 141_700_000, issuedNs: 130 * ms, ptsNs: 1_033_400_000, decodedNs: 135 * ms)
        // A repeat, then a fresh frame: neither pair has a source spacing.
        stats.presented(
            atNs: 150 * ms, issuedNs: 149 * ms, ptsNs: 1_045 * UInt64(ms), decodedNs: 149 * ms,
            isRepeat: true)
        stats.presented(atNs: 158_400_000, issuedNs: 158 * ms, ptsNs: 1_050_100_000, decodedNs: 158 * ms)
        stats.presented(atNs: nil, issuedNs: 160 * ms, ptsNs: 1_066_800_000, decodedNs: 159 * ms)
        stats.decoded(isRepeat: true)
        stats.decoded(isRepeat: false)
        let samples = stats.glassSamples()
        XCTAssertEqual(samples.cadenceErrMs.count, 2)
        XCTAssertEqual(samples.cadenceErrMs[0], 0, accuracy: 0.01)
        XCTAssertEqual(samples.cadenceErrMs[1], 8.3, accuracy: 0.01)
        XCTAssertEqual(samples.displayMs.count, 5)
        XCTAssertEqual(samples.displayMs[0], 2, accuracy: 0.01)
        XCTAssertEqual(samples.repeats.0, 1)
        XCTAssertEqual(samples.repeats.1, 1)
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
