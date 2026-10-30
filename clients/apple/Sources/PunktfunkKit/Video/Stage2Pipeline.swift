// Explicit VideoToolbox decode and presentation orchestration.
//
//   net pump -> VideoDecoder -> tvOS video renderer
//                            -> newest-wins store -> render thread -> CAMetalLayer
//
// Metal rendering is driven by decoded-frame arrival. The ordinary display link supplies a clock,
// retries transient drawable misses, and polls tvOS's displayed IOSurface; deadline pacing instead
// owns a CAMetalDisplayLink. macOS keeps displaySyncEnabled off because synchronized out-of-band
// presents can starve its drawable pool.
//
// The pump, VideoToolbox callback, and Metal render loop run on separate threads. Start, stop, and
// ordinary display-link ticks run on main. Shared stores and clocks are lock-guarded; presenter
// configuration is staged to its render thread. Latency meters stamp receipt, decode, and on-glass
// boundaries. PUNKTFUNK_PRESENT_DEBUG=1 prints Metal pacing diagnostics.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import IOSurface
import Metal
import PunktfunkShared
import QuartzCore
import os

/// PUNKTFUNK_PRESENT_DEBUG=1: the render thread prints a once-per-second line with the decode
/// (ring-submit) rate, present rate, failed/empty wakes and the slowest render call — for
/// diagnosing pacing regressions without instruments. Plain print: the unbundled CLI client's
/// stdout is the cheapest reliable capture channel.
let presentDebug = ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENT_DEBUG"] == "1"

extension Stage2Pipeline {
    /// PUNKTFUNK_DEADLINE_SLACK_MS, as seconds; 0 unless a DEBUG build is asked.
    nonisolated static let deadlineSlack: CFTimeInterval = {
        #if DEBUG
        return (ProcessInfo.processInfo.environment["PUNKTFUNK_DEADLINE_SLACK_MS"]
            .flatMap(Double.init) ?? 0) / 1000
        #else
        return 0
        #endif
    }()
}

/// The pf-present line's os_log mirror (subsystem io.unom.punktfunk, category "present") — the
/// SessionModel "stats" mirror's sibling, so DEADLINE sessions stream their pacing decomposition
/// to Console.app wirelessly with no env var / Xcode attach. Always on for deadline pacing (the
/// stats are a few arrays + one log line per second); other pacings keep the env-gated print.
private let presentLog = ClientLog(category: "present")
/// Pump-side events (loss recovery, format seeding) — the stage-2 sibling of StreamPump's log.
private let pumpLog = ClientLog(category: "pump")

/// Decoded-frame hand-off between the decode half and the render thread. The POLICY is the
/// user's presentation intent (design/apple-presentation-rebuild.md — the 2026-07 rebuild that
/// replaced the visible stage picker):
///
/// - `.newestWins` (Prioritize lowest latency, the default): a 1-slot ring — the decoder
///   overwrites (drops the older undisplayed frame), the render thread takes-and-clears. Zero
///   store by construction: any deeper app-held buffer ahead of a latch-paced display becomes a
///   STANDING queue costing one full refresh per slot, forever (the depth-2 gate post-mortem —
///   see SessionPresenter.gateDepth).
/// - `.fifo(capacity: K)` (Prioritize smoothness): a small deliberate jitter buffer. The
///   decoder appends; overflow drops the OLDEST (bounded added latency — the newest keeps
///   flowing); the render thread pops the oldest ONE per present opportunity, so the cadence is
///   the display's. `take` withholds frames until the buffer has PREROLLED to capacity once —
///   without preroll a steady stream drains every frame on arrival and headroom never builds —
///   and re-arms preroll when it runs dry (an underflow: the previous frame persists on glass,
///   a repeat by omission, while headroom rebuilds). Each buffered frame ≈ one refresh interval
///   of jitter absorbed for one interval of added display latency, which the metrics SHOW —
///   only the OS present floor is shaved from the HUD, never the user's chosen buffer.
///
/// - `.newestTwo` (lowest latency under deadline pacing): keeps the newest two, so the render
///   thread sees a collision and `Carry` decides which shows. `take` hands back the oldest.
///
/// Sendable; lock-guarded — decoder callbacks and the render thread cross here.
public enum FrameStorePolicy: Sendable, Equatable {
    case newestWins
    case fifo(capacity: Int)
    case newestTwo
}

public final class FrameStore<Frame>: @unchecked Sendable {
    private let lock = NSLock()
    let capacity: Int // 1 = newest-wins semantics
    private let isFifo: Bool
    private let keepsTwo: Bool
    private var frames: [Frame] = []
    private var prerolled = false
    /// Submissions since the last `drainSubmitted` — the decode rate for the pf-present line.
    private var submitted = 0
    /// Every submission this session, for the render thread's own rate measure.
    private var total = 0
    /// Smoothness accounting for the pf-present line: frames dropped by a full buffer, and
    /// runs-dry that re-armed preroll.
    private var overflowDrops = 0
    private var underflows = 0

    public init(policy: FrameStorePolicy) {
        switch policy {
        case .newestWins:
            capacity = 1
            isFifo = false
            keepsTwo = false
        case .fifo(let k):
            capacity = max(1, k)
            isFifo = true
            keepsTwo = false
        case .newestTwo:
            capacity = 2
            isFifo = false
            keepsTwo = true
        }
    }

    func submit(_ f: Frame) {
        lock.lock()
        if keepsTwo {
            frames.append(f)
            if frames.count > 2 { frames.removeFirst() }
        } else if isFifo {
            frames.append(f)
            if frames.count > capacity {
                frames.removeFirst() // oldest goes — bounded latency, the newest keeps flowing
                overflowDrops += 1
            }
        } else {
            frames = [f] // newest wins; the replaced frame is the intended drop point
        }
        submitted += 1
        total += 1
        lock.unlock()
    }

    func submittedTotal() -> Int {
        lock.lock()
        defer { lock.unlock() }
        return total
    }

    func drainSubmitted() -> Int {
        lock.lock()
        defer { lock.unlock() }
        let n = submitted
        submitted = 0
        return n
    }

    /// Take-and-reset the smoothness counters (the pf-present `qDrop`/`qDry` stats).
    func drainSmoothing() -> (overflowDrops: Int, underflows: Int) {
        lock.lock()
        defer { lock.unlock() }
        let out = (overflowDrops, underflows)
        overflowDrops = 0
        underflows = 0
        return out
    }

    func take() -> Frame? {
        lock.lock()
        defer { lock.unlock() }
        if keepsTwo { return frames.isEmpty ? nil : frames.removeFirst() }
        if isFifo {
            if !prerolled {
                guard frames.count >= capacity else { return nil } // still building headroom
                prerolled = true
            }
            guard !frames.isEmpty else {
                underflows += 1 // ran dry — repeat by omission, rebuild headroom
                prerolled = false
                return nil
            }
            return frames.removeFirst()
        }
        let f = frames.first
        frames.removeAll(keepingCapacity: true)
        return f
    }

    /// Cadence-driven take: hand back the oldest frame only once its DUE time has arrived, so the
    /// store's job becomes "hold what is not due yet" instead of "release one per present
    /// opportunity" (design/presenter-cadence-rework.md §4.3). `due` projects the frame's due
    /// instant on the same clock as `now`; nil (no cadence estimate for that frame) means due
    /// immediately.
    ///
    /// The preroll gate does not apply here. It exists only to build headroom for a per-slot
    /// drain, and under cadence targeting the cushion IS the headroom — prerolling on top would
    /// stack `capacity − 1` frames of standing latency the user never asked for. `underflows` is
    /// not counted either: an empty store is the normal steady state once frames are held until
    /// due, so the honest starvation signal is `CadenceHealth.late` (the due time had already
    /// passed when the frame became presentable), not a run-dry count.
    func take(dueBy now: CFTimeInterval, due: (Frame) -> CFTimeInterval?) -> Frame? {
        lock.lock()
        defer { lock.unlock() }
        guard let oldest = frames.first else { return nil }
        if let at = due(oldest), at > now { return nil } // held — not due yet
        if isFifo { return frames.removeFirst() }
        frames.removeAll(keepingCapacity: true)
        return oldest
    }

    /// Return a frame the render thread took but could not present (no drawable yet, or a
    /// transient render failure). Newest-wins keeps it only while the slot is still empty — a
    /// newer decoded frame wins; FIFO reinserts it at the FRONT (it is the oldest; a transient
    /// capacity+1 is trimmed by the next submit). Without this, a failed present silently LOSES
    /// the frame, and under the host's infinite GOP a static scene sends no replacement until
    /// the next damage — the stale picture would persist.
    func putBack(_ f: Frame) {
        lock.lock()
        if keepsTwo {
            frames.insert(f, at: 0)
            if frames.count > 2 { frames.removeFirst() } // two newer arrived meanwhile
        } else if isFifo {
            frames.insert(f, at: 0)
        } else if frames.isEmpty {
            frames = [f]
        }
        lock.unlock()
    }

    /// Everything stored, oldest first, leaving the store empty.
    func takeAll() -> [Frame] {
        lock.lock()
        defer { lock.unlock() }
        let all = frames
        frames.removeAll(keepingCapacity: true)
        return all
    }
}

/// The display's vsync grid as last reported by the display link (target timestamp + period,
/// `CACurrentMediaTime` basis), written on main by `renderTick`, read by the render thread to
/// schedule V-Sync-mode presents. A shared box (like `ReadyRing`) so neither thread captures the
/// pipeline itself. Sendable; lock-guarded.
private final class VsyncClock: @unchecked Sendable {
    private let lock = NSLock()
    private var target: CFTimeInterval = 0
    private var period: CFTimeInterval = 0

    func set(target t: CFTimeInterval, period p: CFTimeInterval) {
        lock.lock(); target = t; period = p; lock.unlock()
    }

    /// The next vsync at or after `now`, extrapolated from the last reported phase/period — by
    /// construction less than one period ahead, so a scheduled present can never sit far in the
    /// future holding its drawable. nil (⇒ present immediately) when the link has reported nothing
    /// yet, its period is nonsense, or its data is STALE (an idle/suspended link on an
    /// adaptive-sync display — exactly the case where scheduling onto its grid stalls the stream).
    func nextVsync(after now: CFTimeInterval) -> CFTimeInterval? {
        lock.lock(); defer { lock.unlock() }
        guard period > 0.0005, target > 0, now - target < 0.25 else { return nil }
        if target >= now { return target }
        return target + ceil((now - target) / period) * period
    }

    /// The last reported refresh period, for the pf-present `linkMs` field — 0 until the first
    /// tick (and forever under deadline pacing, which has no ordinary link).
    func lastPeriod() -> CFTimeInterval {
        lock.lock(); defer { lock.unlock() }
        return period
    }
}

/// Selects immediate adaptive presents for sparse input and display-slot presents for dense input.
/// It observes source spacing only; it never computes a due time or delays a frame. Hysteresis keeps
/// 42–48 fps in its current regime, and the 50 ms sample cap prevents one transport hitch from
/// disabling slot pacing at high rate.
struct AdaptiveSlotRegime {
    private var previousPtsNs: UInt64?
    private var intervalNs: Double?
    private(set) var isSlotted = true
    /// The smoothed content interval, once two frames have shown it.
    var interval: CFTimeInterval? { intervalNs.map { $0 / 1e9 } }

    mutating func update(ptsNs: UInt64) -> Bool {
        guard let previousPtsNs else {
            self.previousPtsNs = ptsNs
            return isSlotted
        }
        if ptsNs == previousPtsNs { return isSlotted }
        guard ptsNs > previousPtsNs else {
            self.previousPtsNs = ptsNs
            intervalNs = nil
            isSlotted = true
            return isSlotted
        }
        self.previousPtsNs = ptsNs
        let sample = Double(min(ptsNs - previousPtsNs, 50_000_000))
        let estimate = intervalNs.map { $0 * 0.75 + sample * 0.25 } ?? sample
        intervalNs = estimate
        if isSlotted, estimate >= 24_000_000 { isSlotted = false }
        if !isSlotted, estimate <= 21_000_000 { isSlotted = true }
        return isSlotted
    }
}

/// Arrival and glass pacing's schedule against the ordinary display link, resolved once per
/// session. `PUNKTFUNK_PRESENT_MODE` (`slot`, `immediate`, `vsync`, `adaptive`) picks one for
/// on-device A/B; `adaptive` is the slot regime without the Direct gate.
struct PresentPolicy: Equatable {
    /// Adaptive-refresh latency: sparse input presents at once, dense input one frame per slot.
    var adaptiveSlot: Bool
    /// At most one present per display-link slot.
    var fixedSlot: Bool
    /// Schedule each present on the next vsync.
    var fixedVsync: Bool
    /// Over `adaptiveSlot`: one present in flight, issued at once, while the layer scans out
    /// Direct (`ScanoutPacer`).
    var directGate = false

    static func resolve(
        env: String?, vsync: Bool, vsyncPaced: Bool, adaptiveSlotPaced: Bool
    ) -> PresentPolicy {
        let forcedSlot = env == "slot"
        let adaptiveSlot = adaptiveSlotPaced && !vsync && env != "immediate" && env != "vsync"
            && !forcedSlot
        return PresentPolicy(
            adaptiveSlot: adaptiveSlot,
            fixedSlot: vsyncPaced || forcedSlot,
            fixedVsync: env == "vsync" || (env != "immediate" && vsync),
            directGate: adaptiveSlot && env != "adaptive")
    }

    /// The pace the pf-present line names under `pacing`.
    func label(_ pacing: PresentPacing) -> String {
        switch pacing {
        case .decoded: return "decoded"
        case .deadline: return "deadline"
        case .glass: return "glass"
        case .arrival:
            if directGate { return "direct" }
            return adaptiveSlot ? "adaptive" : fixedSlot ? "slot" : fixedVsync ? "vsync" : "immediate"
        }
    }
}

/// Arrival and glass pacing's slot bookkeeping, owned by the render thread: the display-link
/// target the last present booked and the adaptive regime. At most one frame enters each link
/// slot, so a wake whose target is the booked one waits for the next. The shell reads the
/// clocks and the gates and applies what `fixedSlot` and `plan` return; this decides.
/// Render-thread confined. Internal for tests.
struct ArrivalSlots {
    /// Two targets closer than this are one slot.
    static let sameSlot: CFTimeInterval = 0.002

    /// What the render thread does with the frame it took.
    enum Plan: Equatable {
        /// The adaptive slot already carries a present: return the frame and wait for the next.
        case putBack
        /// Present at `at` (nil: at once). `slot` is the link target this present books.
        /// `holdUnit` is the interval the Direct gate judges a present's hold against: the
        /// content interval, never shorter than the panel can flip.
        case present(at: CFTimeInterval?, slot: CFTimeInterval?, holdUnit: CFTimeInterval)
    }

    let policy: PresentPolicy
    private(set) var lastPresentTarget: CFTimeInterval = 0
    private var regime = AdaptiveSlotRegime()

    init(policy: PresentPolicy) {
        self.policy = policy
    }

    /// The panel's refresh interval, or 120 Hz while the hint carries none.
    static func refresh(panelInterval: CFTimeInterval) -> CFTimeInterval {
        panelInterval > 0 ? panelInterval : 1.0 / 120
    }

    /// The fixed-slot target for a wake at `now`, and whether it is the booked one. A taken
    /// target gates the wake before the store or a gate is touched.
    func fixedSlot(
        now: CFTimeInterval, nextVsync: (CFTimeInterval) -> CFTimeInterval?
    ) -> (target: CFTimeInterval?, taken: Bool) {
        guard policy.fixedSlot, let target = nextVsync(now) else { return (nil, false) }
        return (target, isBooked(target))
    }

    /// What to do with a frame taken at `now`. The regime sees every frame, so its interval
    /// survives Direct stretches. `nextVsync` is asked only when the plan needs the grid; a
    /// stale grid yields no target and presents at once.
    mutating func plan(
        ptsNs: UInt64, dueMediaTime: CFTimeInterval?, now: CFTimeInterval,
        fixedTarget: CFTimeInterval?, direct: Bool, refresh: CFTimeInterval,
        nextVsync: (CFTimeInterval) -> CFTimeInterval?
    ) -> Plan {
        let slotted = policy.adaptiveSlot && regime.update(ptsNs: ptsNs)
        let adaptiveActive = slotted && !direct
        let holdUnit = max(refresh, regime.interval ?? 0)
        let slot = fixedTarget ?? (adaptiveActive ? nextVsync(now) : nil)
        if adaptiveActive, let slot, isBooked(slot) { return .putBack }
        let onGrid = policy.fixedSlot || policy.fixedVsync || adaptiveActive
        let at = onGrid ? slot ?? nextVsync(max(now, dueMediaTime ?? now)) : nil
        return .present(at: at, slot: slot, holdUnit: holdUnit)
    }

    /// The present registered with Metal: its slot is booked.
    mutating func presented(slot: CFTimeInterval?) {
        if let slot { lastPresentTarget = slot }
    }

    private func isBooked(_ target: CFTimeInterval) -> Bool {
        abs(target - lastPresentTarget) < Self.sameSlot
    }
}

/// Selects when a decoded frame enters the layer; decode and the newest-wins store are shared.
///
/// - `arrival` (stage-2): render from frame arrival. This is the macOS default for every codec and
///   an explicit A/B elsewhere; macOS smoothness can additionally schedule it on the vsync grid.
/// - `glass` (stage-3): admit a bounded number of presents and reopen each slot from its on-glass
///   callback. Frames decoded while closed coalesce in the store instead of joining the layer FIFO.
/// - `deadline` (stage-4): pair the newest frame with a CAMetalDisplayLink-vended drawable. This is
///   the iOS default; tvOS retains it as an A/B path because its minimum render window spans two
///   fixed-rate refreshes.
/// - `decoded`: send VideoToolbox's IOSurface-backed output directly to the system video renderer.
///   This is tvOS's latency default and avoids compressed decode buffering plus the Metal FIFO.
public enum PresentPacing: Sendable, Equatable {
    case arrival
    case glass
    case deadline
    case decoded
}

/// Direct decoded-frame handoff to a video layer's system renderer.
///
/// VideoToolbox already produced an IOSurface-backed YUV image, so wrapping it as an immediate
/// uncompressed sample adds no copy or second decode. Backpressure drops the frame instead of
/// building a queue; the next decoder callback supplies a fresher image. Display-link polling maps
/// the renderer's current IOSurface ID back to its capture/decode stamp for on-glass metrics.
/// The renderer owns each sample after enqueue. Sendable because AVSampleBufferVideoRenderer
/// explicitly permits background-thread enqueueing.
final class DecodedVideoSink: @unchecked Sendable {
    struct Stamp {
        let ptsNs: UInt64
        let decodedNs: Int64
        /// Client CLOCK_REALTIME at enqueue — the pf-present `latchMs` origin on this path.
        let submittedNs: Int64
        let isRepeat: Bool
    }

    private let renderer: AVSampleBufferVideoRenderer
    private let lock = NSLock()
    private var stamps: [IOSurfaceID: Stamp] = [:]

    init(renderer: AVSampleBufferVideoRenderer) {
        self.renderer = renderer
    }

    func reset() {
        renderer.flush()
        lock.lock()
        stamps.removeAll()
        lock.unlock()
    }

    @discardableResult
    func submit(_ frame: ReadyFrame) -> Bool {
        guard case .video(let pixelBuffer, _) = frame.image else { return false }
        if renderer.requiresFlushToResumeDecoding || renderer.status == .failed { reset() }
        guard renderer.isReadyForMoreMediaData,
              let sample = Self.immediateSample(pixelBuffer)
        else { return false }
        guard let surfaceID = Self.surfaceID(pixelBuffer) else { return false }
        lock.lock()
        // More than one second of unmatched 60 fps surfaces cannot yield a live latency sample.
        if stamps.count >= 64 { stamps.removeAll(keepingCapacity: true) }
        stamps[surfaceID] = Stamp(
            ptsNs: frame.ptsNs, decodedNs: frame.decodedNs,
            submittedNs: Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: CACurrentMediaTime()),
            isRepeat: frame.flags & PunktfunkConnection.userFlagRepeat != 0)
        lock.unlock()
        renderer.enqueue(sample)
        return true
    }

    /// `PUNKTFUNK_PRESENT_DEBUG`: the renderer's cumulative counters, `optimized` being the frames
    /// shown without compositing — the whole case for this path.
    func logPerformance() {
        guard #available(macOS 14.4, iOS 17.4, tvOS 17.4, *) else { return }
        renderer.loadVideoPerformanceMetrics { m in
            guard let m else { return }
            print(String(
                format: "pf-plane frames=%d dropped=%d corrupted=%d optimized=%d delaySum=%.3f",
                m.totalNumberOfFrames, m.numberOfDroppedFrames, m.numberOfCorruptedFrames,
                m.numberOfFramesDisplayedUsingOptimizedCompositing, m.totalAccumulatedFrameDelay))
            fflush(stdout)
        }
    }

    func takeDisplayedStamp() -> Stamp? {
        guard #available(macOS 14.4, iOS 17.4, tvOS 17.4, *),
              let pixelBuffer = renderer.displayedPixelBuffer(),
              let surfaceID = Self.surfaceID(pixelBuffer)
        else { return nil }
        lock.lock()
        defer { lock.unlock() }
        return stamps.removeValue(forKey: surfaceID)
    }

    private static func surfaceID(_ pixelBuffer: CVPixelBuffer) -> IOSurfaceID? {
        guard let surface = CVPixelBufferGetIOSurface(pixelBuffer) else { return nil }
        return IOSurfaceGetID(surface.takeUnretainedValue())
    }

    static func immediateSample(_ pixelBuffer: CVPixelBuffer) -> CMSampleBuffer? {
        var format: CMVideoFormatDescription?
        guard CMVideoFormatDescriptionCreateForImageBuffer(
            allocator: kCFAllocatorDefault, imageBuffer: pixelBuffer,
            formatDescriptionOut: &format) == noErr,
            let format
        else { return nil }
        var timing = CMSampleTimingInfo(
            duration: .invalid, presentationTimeStamp: .invalid, decodeTimeStamp: .invalid)
        var sample: CMSampleBuffer?
        guard CMSampleBufferCreateReadyWithImageBuffer(
            allocator: kCFAllocatorDefault, imageBuffer: pixelBuffer,
            formatDescription: format, sampleTiming: &timing,
            sampleBufferOut: &sample) == noErr,
            let sample,
            let attachments = CMSampleBufferGetSampleAttachmentsArray(
                sample, createIfNecessary: true),
            CFArrayGetCount(attachments) > 0
        else { return nil }
        let dict = unsafeBitCast(
            CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
        CFDictionarySetValue(
            dict,
            Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
            Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
        return sample
    }
}

/// Reports the decoded frames' HDR state each time it changes. The Welcome only says what was
/// negotiated; the host's encoder can deliver SDR inside it, or flip mid-session. Sendable;
/// lock-guarded.
final class FrameHDRReporter: @unchecked Sendable {
    private let lock = NSLock()
    private var last: Bool?
    private var onChange: (@Sendable (Bool) -> Void)?

    func bind(_ onChange: (@Sendable (Bool) -> Void)?) {
        lock.lock()
        last = nil
        self.onChange = onChange
        lock.unlock()
    }

    func note(_ hdr: Bool) {
        lock.lock()
        let callback = last == hdr ? nil : onChange
        last = hdr
        lock.unlock()
        callback?(hdr)
    }
}

/// Newest-wins 1-slot hand-off box (the generic sibling of `ReadyRing`): deadline pacing's
/// drawable stash — the link thread `put`s each update's vended drawable (replacing an
/// unpresented older one, which just returns to the layer's pool), the render thread `take`s.
/// `putBack` returns a taken value only while the slot is still empty, so a fresher `put` from
/// the other thread is never clobbered by a stale return; `take(where:)` checks the value it
/// takes under the same lock. Internal (not private) for unit tests. Sendable; lock-guarded.
final class LatestBox<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T?
    func put(_ v: T) { lock.lock(); value = v; lock.unlock() }
    func putBack(_ v: T) {
        lock.lock()
        if value == nil { value = v }
        lock.unlock()
    }
    func take() -> T? {
        lock.lock()
        defer { lock.unlock() }
        let v = value
        value = nil
        return v
    }
    func take(where matches: (T) -> Bool) -> T? {
        lock.lock()
        defer { lock.unlock() }
        guard let v = value, matches(v) else { return nil }
        value = nil
        return v
    }
    func peek<R>(_ read: (T) -> R) -> R? {
        lock.lock()
        defer { lock.unlock() }
        return value.map(read)
    }
}

/// A drawable the deadline link vended: when it did, Apple's deadline to call `present()`
/// (`Update.targetTimestamp`), the refresh it is for (all `CACurrentMediaTime`) and the link's
/// refresh period (0 until two updates have run).
struct VendedDrawable {
    let drawable: CAMetalDrawable
    let vendAt: CFTimeInterval
    let deadline: CFTimeInterval
    let target: CFTimeInterval
    let period: CFTimeInterval
}

/// Whether the deadline link is fed enough to park passes: a parked pass that goes unused
/// still stores its drawable, the cost of a frame, so at 60 or 30 fps content every empty
/// vend would burn one. Six of the last eight vends shown opens the gate. Render-thread
/// confined. Internal for tests.
struct LatchGate {
    private var shown: UInt8 = 0
    /// One vend's fate: shown, or expired, skipped or dropped.
    mutating func note(shown fed: Bool) { shown = (shown << 1) | (fed ? 1 : 0) }
    var open: Bool { shown.nonzeroBitCount >= 6 }
}

/// The largest of the last 32 samples of one cost. Lock-free; its owner guards it.
struct RecentMax {
    private var samples = [CFTimeInterval](repeating: 0, count: 32)
    private var next = 0
    var value: CFTimeInterval { samples.max() ?? 0 }
    mutating func note(_ s: CFTimeInterval) {
        samples[next] = s
        next = (next + 1) % samples.count
    }
}

/// The 99th percentile of the last 240 samples of one cost, nil until 120 exist. At 240 the
/// two largest fall above it, so one spike barely moves it. Lock-free; its owner guards it.
struct RecentP99 {
    static let capacity = 240, minimum = 120
    private var samples: [CFTimeInterval] = []
    private var next = 0
    private(set) var value: CFTimeInterval?
    mutating func note(_ s: CFTimeInterval) {
        if samples.count < Self.capacity { samples.append(s) } else { samples[next] = s }
        next = (next + 1) % Self.capacity
        guard samples.count >= Self.minimum else { return }
        let sorted = samples.sorted()
        value = sorted[Int(Double(sorted.count) * 0.99)]
    }
}

/// Deadline pacing's present timing. The link hands each drawable over two refreshes before
/// glass, with the deadline `present()` must beat (`Update.targetTimestamp`).
///
/// - The last safe pick is the deadline less `margin`. The deadline is when the frame must be
///   done: GPU work that ends after it lands a refresh late. So the margin is the worst recent
///   timer wake lateness, + issue → GPU done (encode, the GPU's queue and its work) at p99, +
///   0.5 ms, + a learned extra. A percentile, not the max: one spike costs one late present
///   instead of closing the window. Until that has samples, encode + GPU busy stand in.
/// - The extra learns from late events at the cutoff, counted per `learnWindow` presents. Margin
///   cures a composited layer's late presents; the system's own ignore it. So every 1 ms is on
///   trial and stays only while it pays (`endWindowLocked`).
/// - A drawable paired past the cutoff is never presented: it would land a refresh late and shift
///   every present behind it. The frame waits for the next drawable.
/// - A late present with a later one queued behind it arms one skipped vend, which drains that
///   queue. Presents issued before that skip count for nothing.
///
/// Lock-guarded: the render thread asks, the link thread reads `margin`, Metal's completion
/// threads report.
final class DeadlinePacer: @unchecked Sendable {
    enum Pair: Equatable {
        case skip
        case present(at: CFTimeInterval)
    }

    /// Presents per learning window: 2 s at 120 Hz.
    static let learnWindow = 240

    /// The last safe pick for a drawable vended at `vendAt`: the deadline less `margin`, which
    /// never takes more than 3/4 of the window. A margin past the window would expire every
    /// drawable, and with nothing rendered its measured costs could never come down again.
    static func cutoff(
        vendAt: CFTimeInterval, deadline: CFTimeInterval, margin: CFTimeInterval
    ) -> CFTimeInterval {
        deadline - min(margin, (deadline - vendAt) * 0.75)
    }

    /// A drawable vended at `vendAt`, paired at `now`, with its last safe pick `last`: skip it (a
    /// drain is armed, or `last` has passed), or present at `pick` (latency intent) or now. With
    /// no window left after the vend it presents at once rather than skip every vend.
    static func pair(
        now: CFTimeInterval, vendAt: CFTimeInterval, last: CFTimeInterval, pick: CFTimeInterval,
        latePick: Bool, drain: Bool
    ) -> Pair {
        if drain { return .skip }
        if last <= vendAt { return .present(at: now) }
        if now > last { return .skip }
        return .present(at: latePick ? min(max(pick, now), last) : now)
    }

    private let lock = NSLock()
    private var cpu = RecentMax(), wake = RecentMax(), gpu = RecentMax()
    /// Issue → GPU done. It starts at the issue, so it holds the encode `cpu` measures.
    private var span = RecentP99()
    private var extra: CFTimeInterval = 0
    /// The open learning window: presents observed, late events at the cutoff among them.
    private var windowPresents = 0, windowLate = 0
    /// The late events of the window that added the 1 ms now on trial.
    private var trialFrom: Int?
    private var restWindows = 0, steadyWindows = 0
    private var lastIssuedNs: Int64 = 0
    private var issued = 0, observed = 0
    private var drainPending = false
    private var drainedThroughNs: Int64 = 0
    private var held = 0, superseded = 0, skips = 0, expired = 0, drains = 0, late = 0
    private var dropped = 0, bumps = 0, undone = 0, gpuPastDeadline = 0
    private var vendToDeadline: [Double] = [], deadlineToGlass: [Double] = []

    var margin: CFTimeInterval {
        lock.lock()
        defer { lock.unlock() }
        return marginLocked()
    }

    private func marginLocked() -> CFTimeInterval {
        max(wake.value + (span.value ?? (cpu.value + gpu.value)) + 0.0005, 0.001) + extra
    }

    func noteRender(seconds: CFTimeInterval) { lock.lock(); cpu.note(seconds); lock.unlock() }

    /// One present's GPU work, issued at `issuedAt`, against its deadline; all
    /// `CACurrentMediaTime` seconds.
    func noteGPU(
        issuedAt: CFTimeInterval, start: CFTimeInterval, end: CFTimeInterval,
        deadline: CFTimeInterval
    ) {
        lock.lock()
        gpu.note(end - start)
        span.note(end - issuedAt)
        if end > deadline { gpuPastDeadline += 1 }
        lock.unlock()
    }

    /// A present went out at `ns`. A late present observed later sees it queued behind.
    func noteIssued(ns: Int64) {
        lock.lock()
        lastIssuedNs = max(lastIssuedNs, ns)
        issued += 1
        lock.unlock()
    }

    /// Presents issued and not yet observed on glass or dropped.
    var inFlight: Int {
        lock.lock()
        defer { lock.unlock() }
        return max(issued - observed, 0)
    }

    /// A present issued at `issuedNs` for `targetNs`, whose cutoff sat `cutoffLeadNs` before the
    /// target, reached glass at `presentedNs` (nil: dropped). Any refresh is ≥ 4.17 ms, so 3 ms
    /// past the target is a refresh late. A late present no drain covers is an event: it arms a
    /// drain only when a later present is queued behind it, and teaches only from the cutoff.
    /// Returns the late present's count in this window line, 0 when it was not late.
    @discardableResult
    func observe(
        issuedNs: Int64, presentedNs: Int64?, targetNs: Int64, cutoffLeadNs: Int64
    ) -> Int {
        lock.lock()
        defer { lock.unlock() }
        observed += 1
        var lateCount = 0
        if let presentedNs, presentedNs - targetNs > 3_000_000 {
            late += 1
            lateCount = late
            if issuedNs > drainedThroughNs, !drainPending {
                if lastIssuedNs > issuedNs { drainPending = true }
                if targetNs - issuedNs < cutoffLeadNs + 1_000_000 { windowLate += 1 }
            }
        } else if presentedNs == nil {
            dropped += 1
        }
        windowPresents += 1
        if windowPresents >= Self.learnWindow { endWindowLocked() }
        return lateCount
    }

    /// Closes a learning window. Six late events at the cutoff or more add 1 ms on trial: the
    /// next window must count under half as many, or the 1 ms goes and learning rests ten
    /// windows. Margin cures a composited layer's late presents at once; the system's own ignore
    /// it, and so does a cutoff at its cap. Below six, half is lost in a quiet window's noise.
    /// Every fifth window with no change gives 0.5 ms back, whatever the late rate: a margin still
    /// needed is relearned from the late presents its loss brings.
    private func endWindowLocked() {
        let count = windowLate
        windowPresents = 0
        windowLate = 0
        if let before = trialFrom {
            trialFrom = nil
            if 2 * count >= before {
                extra = max(0, extra - 0.001)
                undone += 1
                restWindows = 10
                steadyWindows = 0
                return
            }
        }
        if restWindows > 0 {
            restWindows -= 1
        } else if count >= 6 {
            extra += 0.001
            trialFrom = count
            bumps += 1
            steadyWindows = 0
            return
        }
        steadyWindows += 1
        if steadyWindows >= 5, extra > 0 {
            extra = max(0, extra - 0.0005)
            steadyWindows = 0
        }
    }

    /// The armed drain, once: presents issued before `nowNs` were queued behind the late one.
    func takeDrain(nowNs: Int64) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard drainPending else { return false }
        drainPending = false
        drainedThroughNs = nowNs
        drains += 1
        return true
    }

    func noteHeld(wakeLate: CFTimeInterval, superseded newer: Bool) {
        lock.lock()
        held += 1
        if newer { superseded += 1 }
        wake.note(max(wakeLate, 0))
        lock.unlock()
    }

    func noteSkip() { lock.lock(); skips += 1; lock.unlock() }
    func noteExpired() { lock.lock(); expired += 1; lock.unlock() }

    /// One vend's geometry, for the window line.
    func noteVend(at vendAt: CFTimeInterval, deadline: CFTimeInterval, target: CFTimeInterval) {
        lock.lock()
        vendToDeadline.append((deadline - vendAt) * 1000)
        deadlineToGlass.append((target - deadline) * 1000)
        lock.unlock()
    }

    /// The once-a-second window line; resets the window counters. `troubled`: a present in the
    /// window landed late or was dropped.
    func windowLine() -> (text: String, troubled: Bool) {
        lock.lock()
        defer { lock.unlock() }
        func p50(_ xs: [Double]) -> Double { xs.isEmpty ? 0 : xs.sorted()[xs.count / 2] }
        let troubled = late > 0 || dropped > 0
        let line = String(
            format: "pf-deadline marginMs=%.2f extraMs=%.1f wakeMs=%.2f cpuMs=%.2f gpuMs=%.2f "
                + "renderMs=%.2f gpuPastDeadline=%d "
                + "held=%d superseded=%d skips=%d expired=%d drains=%d late=%d dropped=%d "
                + "bumps=%d undone=%d vendToDeadlineMs=%.2f deadlineToGlassMs=%.2f",
            marginLocked() * 1000, extra * 1000, wake.value * 1000, cpu.value * 1000,
            gpu.value * 1000, (span.value ?? -0.001) * 1000, gpuPastDeadline, held, superseded,
            skips, expired, drains, late, dropped, bumps, undone, p50(vendToDeadline),
            p50(deadlineToGlass))
        held = 0; superseded = 0; skips = 0; expired = 0; drains = 0; late = 0; dropped = 0
        bumps = 0; undone = 0; gpuPastDeadline = 0
        vendToDeadline.removeAll(keepingCapacity: true)
        deadlineToGlass.removeAll(keepingCapacity: true)
        return (line, troubled)
    }
}

/// One late deadline present, as its `pf-late` line. Times are `CACurrentMediaTime` seconds,
/// the GPU span's too; offsets from the deadline are positive after it. Internal for tests.
struct LatePresent {
    /// Glass past the drawable's target.
    let lateNs: Int64
    let vendAt: CFTimeInterval, issuedAt: CFTimeInterval, deadline: CFTimeInterval
    /// nil until the command buffer completes.
    let gpu: (start: CFTimeInterval, end: CFTimeInterval)?
    /// The thread waited for the pick; the pick showed the older of two frames.
    let held: Bool, carried: Bool
    /// What became of the drawable before this one: presented, expired or skipped.
    let after: String
    let inFlight: Int
    /// nil without a decode stamp.
    let decodeToIssueNs: Int64?

    var line: String {
        func ms(_ s: Double) -> String { String(format: "%.2f", s * 1000) }
        func na(_ s: Double?) -> String { s.map(ms) ?? "na" }
        return "pf-late lateMs=\(ms(Double(lateNs) / 1e9))"
            + " issueToDeadlineMs=\(ms(deadline - issuedAt))"
            + " gpuStartMs=\(na(gpu.map { $0.start - deadline }))"
            + " gpuEndMs=\(na(gpu.map { $0.end - deadline }))"
            + " held=\(held ? 1 : 0) carried=\(carried ? 1 : 0) after=\(after)"
            + " inFlight=\(inFlight) ageMs=\(ms(issuedAt - vendAt))"
            + " decodeToIssueMs=\(na(decodeToIssueNs.map { Double($0) / 1e9 }))"
    }
}

/// Where decoded frames land in the refresh, how much they jitter, and which refresh each maps to.
///
/// Phase is the arrival's offset after its vend as a fraction of the period, over the last 2 s
/// (240 frames at 120 Hz). Once settled, an arrival more than 3σ off the mean is not sampled; a
/// half second of nothing but such arrivals means the phase moved, and the estimate restarts.
///
/// A pick either shows the frame in the refresh it arrived in (`same`, pick after the arrival)
/// or in the next (`next`, pick before it). The point inside the window changes no glass time,
/// only how far the pick sits from arrivals, so it sits mid-gap where the window allows. Drift
/// walks the phase toward the window's edge; switching mapping costs exactly one drop or repeat,
/// so a switch needs a clear win and 2 s since the last, unless the current pick is inside 1σ.
/// Render-thread confined. Internal for tests.
struct PhaseTracker {
    enum Mapping { case same, next }

    static let capacity = 240
    static let dwell: CFTimeInterval = 2
    private var samples: [Double] = []
    private var nextSlot = 0
    private var rejectedRun = 0
    private(set) var mapping: Mapping = .same
    private var lastSwitch: CFTimeInterval = -.infinity
    /// Window counters for the debug line.
    private(set) var switches = 0, rejected = 0

    mutating func note(fraction: Double) {
        let f = fraction - fraction.rounded(.down)
        if let est = estimate() {
            let d = abs(f - est.mean)
            if min(d, 1 - d) > max(3 * est.sigma, 0.02) {
                rejected += 1
                rejectedRun += 1
                if rejectedRun >= Self.capacity / 4 { reset() }
                return
            }
        }
        rejectedRun = 0
        if samples.count < Self.capacity {
            samples.append(f)
        } else {
            samples[nextSlot] = f
        }
        nextSlot = (nextSlot + 1) % Self.capacity
    }

    private mutating func reset() {
        samples.removeAll(keepingCapacity: true)
        nextSlot = 0
        rejectedRun = 0
    }

    /// Circular mean and σ (the p90 distance from it ÷ 1.645), both as fractions of the period;
    /// nil until half full.
    func estimate() -> (mean: Double, sigma: Double)? {
        guard samples.count >= Self.capacity / 2 else { return nil }
        var sx = 0.0, sy = 0.0
        for f in samples {
            sx += cos(2 * .pi * f)
            sy += sin(2 * .pi * f)
        }
        var mean = atan2(sy, sx) / (2 * .pi)
        if mean < 0 { mean += 1 }
        let dist = samples.map { f -> Double in
            let d = abs(f - mean)
            return min(d, 1 - d)
        }.sorted()
        return (mean, dist[dist.count * 9 / 10] / 1.645)
    }

    /// The pick's offset after the vend, in `0...window`, for arrivals `a` after it with jitter
    /// `sigma` (all seconds, period `p`), and whether the mapping switched — a switch to `same`
    /// leaves two frames at this pick, and the older is the one that must go.
    mutating func pick(
        arrival a: Double, sigma: Double, period p: Double, window w: Double, now: CFTimeInterval
    ) -> (offset: Double, switched: Bool) {
        let w = max(w, 0)
        func clearance(_ h: Double) -> Double {
            min(abs(h - (a - p)), abs(h - a), abs(h - (a + p)))
        }
        let same = min(a + p / 2, w)
        let before = min(max(a - p / 2, 0), w)
        let sameOK = same > a
        let cSame = sameOK ? clearance(same) : -.infinity
        let cNext = clearance(before)
        let need = max(3 * sigma, 0.001)
        let hysteresis = max(sigma, 0.0005)
        // `same` shows the frame a refresh sooner: keep it while it clears by `need`, come back
        // to it once it clears by `need` + hysteresis.
        var want = mapping
        switch mapping {
        case .same:
            if !sameOK || cSame < sigma { want = cNext > cSame ? .next : .same }
            else if cSame < need, cNext > cSame + hysteresis { want = .next }
        case .next:
            if cNext < sigma, cSame > cNext { want = .same }
            else if sameOK, cSame >= need + hysteresis { want = .same }
        }
        let forced = (mapping == .same && (!sameOK || cSame < sigma))
            || (mapping == .next && cNext < sigma)
        var switched = false
        if want != mapping, forced || now - lastSwitch >= Self.dwell {
            mapping = want
            lastSwitch = now
            switches += 1
            switched = true
        }
        return (mapping == .same && sameOK ? same : before, switched)
    }

    /// Take-and-reset the window counters.
    mutating func drainCounters() -> (switches: Int, rejected: Int) {
        defer { switches = 0; rejected = 0 }
        return (switches, rejected)
    }
}

/// The bounded one-frame hold. Two frames between picks: show the older and keep the newer for
/// the next pick, so a burst lands on consecutive refreshes instead of losing one. A pick with
/// nothing new shows the kept frame and ends the hold. A phase switch is the scheduled drift
/// glitch and never carries.
///
/// A host slower than the panel ends every hold by itself within a beat, so `maxRun` there
/// covers one beat (`run(forSlowBy:period:)`). A host at or above panel rate may never leave a
/// gap: 24 picks in a row (≈ 200 ms at 120 Hz) is a standing queue, and that pick shows the
/// newest. Render-thread confined.
struct Carry {
    static let maxRun = 24
    private(set) var run = 0
    /// Window counters for the debug line: picks that kept a frame, holds ended by the bound.
    private(set) var carried = 0, forcedDrains = 0

    /// The bound for a host `slowBy` Hz under a panel of `period`: one beat (capped at 2 s)
    /// when the host is measurably slower, `maxRun` otherwise.
    static func run(forSlowBy slowBy: Double, period: CFTimeInterval) -> Int {
        guard slowBy > 0.3, period > 0 else { return maxRun }
        return max(maxRun, Int(min(2.0, 1.2 / slowBy) / period))
    }

    /// `frames` in arrival order, non-empty: what to show now and what to keep.
    mutating func choose<F>(
        _ frames: [F], allowed: Bool, maxRun: Int = Carry.maxRun
    ) -> (show: F, keep: F?) {
        let n = frames.count
        if n == 1 {
            run = 0
            return (frames[0], nil)
        }
        if !allowed || run >= maxRun {
            if run >= maxRun { forcedDrains += 1 }
            run = 0
            return (frames[n - 1], nil)
        }
        run += 1
        carried += 1
        return (frames[n - 2], frames[n - 1])
    }

    mutating func drainCounters() -> (carried: Int, forcedDrains: Int) {
        defer { carried = 0; forcedDrains = 0 }
        return (carried, forcedDrains)
    }
}

/// Deadline pacing's choices per vended drawable, owned by the render thread: where in the
/// window the present goes (`PhaseTracker`, then `DeadlinePacer.pair`), and which of the frames
/// decoded by then shows (`Carry`, bounded by the host's measured rate). Under the latency
/// intent (`latePick`) the pick and the carry apply; Smoothness presents on pairing.
/// The shell owns the ring, the waits and the pacer. Render-thread confined. Internal for tests.
struct DeadlineSelector {
    enum Pairing: Equatable {
        /// This drawable goes back unpresented; the frame waits for the next vend.
        case skip
        /// Present at `at` (`CACurrentMediaTime`). `switched`: the phase mapping just changed,
        /// so the pick never carries a frame.
        case present(at: CFTimeInterval, switched: Bool)
    }

    /// The host's rate is re-measured over this long.
    static let rateWindow: CFTimeInterval = 2

    let latePick: Bool
    private var phase = PhaseTracker()
    private var carry = Carry()
    private var rateMark: (at: CFTimeInterval, frames: Int)
    private var hostHz = 0.0
    private var carryBound = Carry.maxRun
    private var lastPeriod: CFTimeInterval = 0
    /// A mapping switch found by a pairing that then skipped: the next present must not
    /// carry a frame either, so it waits here for that present.
    private var pendingSwitch = false

    init(latePick: Bool, now: CFTimeInterval) {
        self.latePick = latePick
        rateMark = (now, 0)
    }

    /// Pair a drawable vended at `vendAt` (refresh `period`, 0 until two updates ran) with a
    /// frame at `now`, `last` being its last safe pick and `drain` an armed drain skip. The
    /// phase pick runs, and may switch the mapping, even when the pairing then skips; the
    /// switch then rides the next present.
    mutating func pair(
        vendAt: CFTimeInterval, period: CFTimeInterval, last: CFTimeInterval,
        now: CFTimeInterval, drain: Bool
    ) -> Pairing {
        var pick = last
        var switched = false
        lastPeriod = period
        if latePick, period > 0, let est = phase.estimate(), est.sigma < 0.25 {
            let choice = phase.pick(
                arrival: est.mean * period, sigma: est.sigma * period, period: period,
                window: last - vendAt, now: now)
            pick = vendAt + choice.offset
            switched = choice.switched
        }
        let paired = DeadlinePacer.pair(
            now: now, vendAt: vendAt, last: last, pick: pick, latePick: latePick, drain: drain)
        let switching = switched || pendingSwitch
        guard case .present(let at) = paired else {
            pendingSwitch = switching
            return .skip
        }
        pendingSwitch = false
        return .present(at: at, switched: switching)
    }

    /// At the pick, before the frames are gathered: every `rateWindow` the host's frame rate
    /// against the panel's (`period`) sets the carry's bound. `submitted` counts the frames the
    /// ring has taken in, asked only when the window is up.
    mutating func noteRate(at t: CFTimeInterval, period: CFTimeInterval, submitted: () -> Int) {
        guard t - rateMark.at >= Self.rateWindow, period > 0 else { return }
        let n = submitted()
        hostHz = Double(n - rateMark.frames) / (t - rateMark.at)
        carryBound = Carry.run(forSlowBy: 1 / period - hostHz, period: period)
        rateMark = (t, n)
    }

    /// `frames` in arrival order, non-empty: the one to show and the one to keep for the next pick.
    mutating func choose<F>(_ frames: [F], switched: Bool) -> (show: F, keep: F?) {
        carry.choose(frames, allowed: !switched, maxRun: carryBound)
    }

    /// The shown frame's arrival as a fraction of the period after its vend, once the stamps
    /// are real. `vendNs` is asked only then.
    mutating func noteShown(decodedNs: Int64, period: CFTimeInterval, vendNs: () -> Int64) {
        guard period > 0, decodedNs > 0 else { return }
        phase.note(fraction: Double(decodedNs - vendNs()) / (period * 1e9))
    }

    /// Ends the debug window: resets the phase and carry counters, and returns the line's tail
    /// when `report`. `composition` is asked only then.
    mutating func endWindow(report: Bool, composition: () -> String) -> String? {
        let p = phase.drainCounters()
        let c = carry.drainCounters()
        guard report else { return nil }
        let sigmaMs = phase.estimate().map { $0.sigma * lastPeriod * 1000 } ?? -1
        return String(
            format: " carried=%d carryDrains=%d carryBound=%d hostHz=%.1f "
                + "phaseSwitch=%d phaseRejected=%d sigmaMs=%.2f mapping=%@ comp=%@",
            c.carried, c.forcedDrains, carryBound, hostHz, p.switches, p.rejected, sigmaMs,
            phase.mapping == .same ? "same" : "next", composition())
    }
}

/// Stage-4's stale-link ladder. A link that stops vending is relinked; a link that stalls
/// AGAIN within `rebuildWindow` of that relink means the CAMetalLayer itself is wedged, so
/// the caller is told and its `WedgePolicy` picks the cure. A wedged layer's relink vends
/// its pool once and is silent again within ≈0.3 s; 2 s covers that with margin, and a link
/// that ran clean for longer has earned a fresh relink. Render-thread confined. Internal
/// for tests.
struct LinkStallPolicy {
    enum Action { case relink, rebuild }
    let rebuildWindow: CFTimeInterval
    private(set) var lastRelink: CFTimeInterval = -.infinity
    init(rebuildWindow: CFTimeInterval = 2) { self.rebuildWindow = rebuildWindow }
    mutating func onStall(now: CFTimeInterval) -> Action {
        if now - lastRelink < rebuildWindow { return .rebuild }
        lastRelink = now
        return .relink
    }
}

/// Stage-4's stale-link watchdog. Every drawable arrives from the link, so one that stays
/// silent for `staleAfter` while a decoded frame waits freezes the picture, with audio and input
/// alive. 250 ms ≈ 30 refreshes at 120 Hz; a false positive costs one relinked frame.
///
/// A stall relinks. A stall `LinkStallPolicy` reads as a wedged layer is reported once, and
/// the watchdog then holds, the fresh link staying, until the system shows a frame again
/// (`rearm`): a vend alone proves nothing, since a wedged layer vends its pool once. The caller
/// starts the links and does the reporting. Render-thread confined. Internal for tests.
struct LinkWatchdog {
    enum Action: Equatable {
        case none
        /// Retire the link and start a fresh one.
        case relink(stalledMs: Int)
        /// Relink, and report the layer wedged.
        case wedged(stalledMs: Int)
    }

    static let staleAfter: CFTimeInterval = 0.25

    private(set) var lastVend: CFTimeInterval
    private(set) var isWedged = false
    private var ladder = LinkStallPolicy()

    /// A fresh link gets its first vend before it can be judged: start one at `now`.
    init(now: CFTimeInterval) {
        lastVend = now
    }

    /// The link handed over a drawable, or was just (re)started.
    mutating func noteVend(at now: CFTimeInterval) {
        lastVend = now
    }

    /// A decoded frame found no drawable. `linkRunning`: the first frame has started a link.
    /// `canReport`: someone listens for a wedge, else a repeat stall relinks again.
    mutating func noVend(linkRunning: Bool, canReport: Bool, now: CFTimeInterval) -> Action {
        let stalledFor = now - lastVend
        guard linkRunning, !isWedged, stalledFor > Self.staleAfter else { return .none }
        lastVend = now
        let ms = Int(stalledFor * 1000)
        guard ladder.onStall(now: now) == .rebuild, canReport else { return .relink(stalledMs: ms) }
        isWedged = true
        return .wedged(stalledMs: ms)
    }

    /// A wedged watchdog re-arms once `glass` (asked only while wedged) reports a present the
    /// system showed. True when it did.
    mutating func rearm(glass: () -> Bool) -> Bool {
        guard isWedged, glass() else { return false }
        isWedged = false
        ladder = LinkStallPolicy()
        return true
    }
}

/// Stage-3's present gate: admits `capacity` in-flight (presented, not yet on glass) drawables.
/// The render thread `tryAcquire`s before taking a frame; the drawable's presented handler
/// `release`s and re-signals the render thread. Depth 1 fully serializes presents on the on-glass
/// callback — which costs a refresh whenever the callback's own latency pushes the next present
/// past a vsync; depth 2 keeps one flip queued behind the one scanning out, so a decoded frame
/// presents immediately and latches the very next vsync while the queue still can't build (see
/// `SessionPresenter.gateDepth` for the per-platform choice). `staleAfter` is insurance against a
/// present whose handler never fires (the macOS "out-of-band presents aren't damage" hazard class
/// — see MetalVideoPresenter's init post-mortem): rather than freezing the stream, a full gate
/// force-opens a slot 100 ms after its oldest present, a visible ~10 fps degradation that
/// PUNKTFUNK_PRESENT_DEBUG's `forced` counter exposes (it reads 0 on healthy systems). Internal
/// (not private) for unit tests. Sendable; lock-guarded — the releaser runs on a Metal callback
/// thread.
final class PresentGate: @unchecked Sendable {
    /// How long one pending present may hold its slot before it's presumed lost.
    static let staleAfter: CFTimeInterval = 0.1

    private let lock = NSLock()
    private let capacity: Int
    /// Arm instants of the in-flight presents, oldest first (≤ `capacity` entries).
    private var armed: [CFTimeInterval] = []
    private var forced = 0

    /// `capacity` = the in-flight present budget (clamped to ≥ 1) — see the type doc.
    init(capacity: Int = 1) {
        self.capacity = max(1, capacity)
    }

    /// Arm the gate for one present. False = the gate is full of live presents (none stale) —
    /// leave the frame in the ring; a presented handler's release/re-signal (or the next
    /// display-link tick) retries with the freshest frame then.
    func tryAcquire(now: CFTimeInterval) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if armed.count >= capacity {
            // Full: reopen only by presuming the OLDEST in-flight present lost (its handler
            // never fired) rather than stalling the stream.
            guard let oldest = armed.first, now - oldest > Self.staleAfter else { return false }
            armed.removeFirst()
            forced += 1
        }
        armed.append(now)
        return true
    }

    /// One in-flight present reached glass (or was dropped, or its render failed before a present
    /// was registered) — free the oldest slot. A release with nothing in flight is a no-op; a
    /// lost present's handler firing late after its stale force-open can transiently over-admit
    /// one flip, which the next glass callback corrects.
    func release() {
        lock.lock()
        if !armed.isEmpty { armed.removeFirst() }
        lock.unlock()
    }

    /// Take-and-reset the force-open count (PUNKTFUNK_PRESENT_DEBUG's `forced` stat).
    func drainForced() -> Int {
        lock.lock()
        defer { lock.unlock() }
        let n = forced
        forced = 0
        return n
    }
}

extension MetalVideoPresenter {
    /// One present tail, two decode sources: the VideoToolbox biplanar buffer or the PyroWave
    /// Metal planes. The ring, pacing and meters are agnostic to which.
    fileprivate func render(
        _ image: ReadyImage, presentAtMediaTime: CFTimeInterval? = nil,
        into drawable: CAMetalDrawable? = nil, onPresented: ((Int64?) -> Void)? = nil
    ) -> Bool {
        switch image {
        case .video(let pixelBuffer, let isHDR):
            return render(
                pixelBuffer, isHDR: isHDR, presentAtMediaTime: presentAtMediaTime,
                into: drawable, onPresented: onPresented)
        case .planar(let planes):
            return renderPlanar(
                planes, presentAtMediaTime: presentAtMediaTime, into: drawable,
                onPresented: onPresented)
        }
    }

    /// Size and colour the layer for `image` before a vend can succeed on it.
    fileprivate func reconcileLayer(for image: ReadyImage) {
        switch image {
        case .video(let pixelBuffer, let isHDR):
            reconcileLayer(
                decodedSize: CGSize(
                    width: CVPixelBufferGetWidth(pixelBuffer),
                    height: CVPixelBufferGetHeight(pixelBuffer)),
                isHDR: isHDR, tenBitSDR: MetalVideoPresenter.tenBitBuffer(pixelBuffer))
        case .planar(let planes):
            reconcileLayer(
                decodedSize: CGSize(width: planes.width, height: planes.height), isHDR: planes.pq)
        }
    }
}

/// The CAMetalDisplayLink delegate for deadline pacing: each per-refresh update stashes its
/// vended drawable (newest wins) and nudges the render thread — which also wakes on decoder
/// arrivals, so whichever half completes the (frame, drawable) pair triggers the present. Also
/// applies the staged frame-rate hint and the presenter's queued layer writes from the link's own
/// thread. Retained by the link thread's closure (the link holds it weak); captures only the shared
/// boxes and the presenter, never the pipeline — the same no-self-capture rule as the pump/render
/// threads.
private final class DeadlineLinkDelegate: NSObject, CAMetalDisplayLinkDelegate {
    private let presenter: MetalVideoPresenter
    private let stash: LatestBox<VendedDrawable>
    private let renderSignal: DispatchSemaphore
    private let hint: FrameRateHint
    private let stats: PresentDebugStats?
    /// Every update's vend→glass lead goes to the overlay as an OS-floor sample; its p50 is the
    /// floor iOS and tvOS take off the shown display and end-to-end. ~1 refresh is the goal; ~2
    /// means the compositor runs a frame ahead of us. Tracks VRR rate changes.
    private let hud: HudSink?
    /// The pool depth this session vends from (`startDeadlinePresenter` sets it on the layer).
    private let drawableCount: Int
    private let pacer: DeadlinePacer
    /// The latency intent picks at the deadline; Smoothness presents on pairing.
    private let latePick: Bool
    private var loggedLinkUp = false
    /// The refresh period: the shortest of the last 16 target steps, so a skipped update (two
    /// refreshes) never counts and a rate change lands within 16.
    private var lastTarget: CFTimeInterval = 0
    private var steps = [CFTimeInterval](repeating: .infinity, count: 16)
    private var stepIndex = 0

    /// Apple accepts 1 or 2 here; 1 is the shorter.
    static let frameLatency: Float = 1

    init(
        presenter: MetalVideoPresenter,
        stash: LatestBox<VendedDrawable>, renderSignal: DispatchSemaphore,
        hint: FrameRateHint, stats: PresentDebugStats?, hud: HudSink?,
        drawableCount: Int, pacer: DeadlinePacer, latePick: Bool
    ) {
        self.presenter = presenter
        self.stash = stash
        self.renderSignal = renderSignal
        self.hint = hint
        self.stats = stats
        self.hud = hud
        self.drawableCount = drawableCount
        self.pacer = pacer
        self.latePick = latePick
    }

    func metalDisplayLink(_ link: CAMetalDisplayLink, needsUpdate update: CAMetalDisplayLink.Update) {
        presenter.applyLayerWrites()
        if let range = hint.drain(), link.preferredFrameRateRange != range {
            link.preferredFrameRateRange = range
        }
        // Published every update: a mode switch re-applies the range mid-session.
        let range = link.preferredFrameRateRange
        PresentLinkInfo.shared.publish(
            ask: Self.frameLatency, latency: link.preferredFrameLatency, rangeMin: range.minimum,
            rangeMax: range.maximum, drawables: drawableCount)
        if !loggedLinkUp {
            loggedLinkUp = true
            let msg = String(
                format: "deadline link up: preferredFrameLatency=%.2f maxDrawables=%d "
                    + "range=%.0f-%.0f preferred=%.0f latePick=%d",
                link.preferredFrameLatency, drawableCount, range.minimum, range.maximum,
                range.preferred ?? 0, latePick ? 1 : 0)
            presentLog.info("\(msg, privacy: .public)")
        }
        // The link's own pipeline depth, measured: how far ahead of glass this vend runs.
        let vendAt = CACurrentMediaTime()
        let deadline = update.targetTimestamp
        let target = update.targetPresentationTimestamp
        let leadS = target - vendAt
        stats?.vendLead(ms: leadS * 1000)
        pacer.noteVend(at: vendAt, deadline: deadline, target: target)
        // The OS floor the overlay takes off end-to-end: how far before glass the present goes
        // out — from the pick at the deadline, else from the vend.
        let floorS = latePick
            ? target - DeadlinePacer.cutoff(vendAt: vendAt, deadline: deadline, margin: pacer.margin)
            : leadS
        if floorS > 0 { hud?.floor(ns: Int64(floorS * 1_000_000_000)) }
        let step = target - lastTarget
        if step > 0.002, step < 0.1 {
            steps[stepIndex] = step
            stepIndex = (stepIndex + 1) % steps.count
        }
        lastTarget = target
        let period = steps.min().flatMap { $0.isFinite ? $0 : nil } ?? 0
        stash.put(VendedDrawable(
            drawable: update.drawable, vendAt: vendAt, deadline: deadline, target: target,
            period: period))
        renderSignal.signal()
    }
}


/// Bridges the VideoToolbox decode-completion callback to the core Automatic-bitrate controller's
/// decode signal. Created as a pipeline property so the decoder's `onDecoded` callback (built in
/// `init`, before the connection exists) can capture it, then `start` binds the live connection +
/// the arming flag once known — the same "reference captured in init, configured in start" shape as
/// `HudSink`. `record` runs on VideoToolbox's callback thread; `bind` runs once on the main
/// thread before the pump feeds the first AU, so the plain fields are safe (set-once, then read).
private final class DecodeReport: @unchecked Sendable {
    private weak var connection: PunktfunkConnection?
    private var enabled = false
    func bind(_ connection: PunktfunkConnection) {
        self.connection = connection
        self.enabled = connection.wantsDecodeLatency()
    }
    /// Report received→decoded for one frame, in µs. Both stamps are client `CLOCK_REALTIME`
    /// (no skew). Skips when the controller isn't armed, so it's free to call on every decode.
    func record(receivedNs: Int64, decodedNs: Int64) {
        guard enabled, let c = connection else { return }
        let us = (decodedNs - receivedNs) / 1000
        if us > 0 { c.reportDecodeUs(UInt32(min(us, Int64(UInt32.max)))) }
    }
}

/// The stats overlay's stamp sink. The decode callback is built in `init`, before a connection
/// exists, so it writes through this. `start` binds the connection on the main thread before the
/// first AU, so the plain field is safe (set once, then read) — DecodeReport's shape.
final class HudSink: @unchecked Sendable {
    private weak var connection: PunktfunkConnection?
    func bind(_ connection: PunktfunkConnection) { self.connection = connection }
    func decoded(ptsNs: UInt64, receivedNs: Int64, decodedNs: Int64) {
        connection?.hudDecoded(ptsNs: ptsNs, receivedNs: receivedNs, decodedNs: decodedNs)
    }
    func displayed(ptsNs: UInt64, decodedNs: Int64, atNs: Int64) {
        connection?.hudDisplayed(ptsNs: ptsNs, decodedNs: decodedNs, displayedNs: atNs)
    }
    func floor(ns: Int64) { connection?.hudOsFloor(ns: ns) }
}

/// The decode callbacks' way to the core's loss recovery, which owns the freeze and every ask.
/// Built in `init`, before a connection exists: `start` binds the session's and `stop` unbinds
/// it, so a late callback reports nothing. The callbacks run on VideoToolbox threads, hence the
/// lock. `refusing` gives a run of refusals its one log line.
private final class RecoveryReport: @unchecked Sendable {
    private let lock = NSLock()
    private var connection: PunktfunkConnection?
    private var refusing = false

    func bind(_ c: PunktfunkConnection?) {
        lock.lock(); connection = c; refusing = false; lock.unlock()
    }

    /// One decoded picture. True: hold the last good one instead. Unbound, everything shows.
    func decoded(flags: UInt32) -> Bool {
        lock.lock()
        refusing = false
        let c = connection
        lock.unlock()
        return c?.recoveryOnDecoded(flags: flags) ?? false
    }

    /// One refused AU. True on the first since a decoded picture.
    func refused() -> Bool {
        lock.lock()
        let first = !refusing
        refusing = true
        let c = connection
        lock.unlock()
        c?.recoveryOnNoOutput()
        return first
    }
}

@MainActor
public final class Stage2Pipeline {
    private let ring: FrameStore<ReadyFrame>
    private let presenter: MetalVideoPresenter
    private let decoder: VideoDecoder
    private let decodedSink: DecodedVideoSink?
    /// When `renderTick` last logged the decoded sink's metrics. Main thread.
    private var lastPlaneLine: CFTimeInterval = 0
    /// Presentation mechanism, fixed for the pipeline lifetime and resolved per session by
    /// SessionPresenter. See PresentPacing for each path's ownership and cadence.
    private let pacing: PresentPacing
    /// The glass gate's in-flight present budget (`PresentGate` capacity) — meaningful only under
    /// `.glass`; SessionPresenter resolves it per platform (see `SessionPresenter.gateDepth`).
    private let gateDepth: Int
    /// macOS smoothness: schedule at most one present on each display-link target so the FIFO
    /// store drains at display cadence. Deadline pacing has its own link and ignores this policy.
    private let vsyncPaced: Bool
    /// macOS adaptive-refresh latency path: immediate sparse input, one drawable per dense slot.
    private let adaptiveSlotPaced: Bool
    /// Source-timestamp playout for the SMOOTHNESS intent: every decoded frame is stamped with
    /// when it is due on the host's own cadence. `nil` under latency, whose path presents on
    /// arrival without cadence arithmetic.
    private let cadence: CadenceClock?
    private let endToEndMeter: LatencyMeter?
    /// The stats overlay's decode, display and OS-floor stamps; `start` binds the connection.
    private let hud = HudSink()
    /// The decoder callbacks' reports to the core's loss recovery; `start` binds the connection.
    private let recovery = RecoveryReport()
    /// Feeds the core Automatic-bitrate controller's decode signal from the decode callback; `start`
    /// binds the live connection + arming flag (see DecodeReport).
    private let decodeReport = DecodeReport()
    private let frameHDR = FrameHDRReporter()
    private var token = StopFlag()
    /// LIVE host↔client clock offset, read AT EACH RECORD — never cached per session. Until
    /// 2026-08-13 this was a `let` snapshot of the connect-time handshake, and on a host whose
    /// wall clock steps (a VM under NTP) the frozen value silently shifted every host-anchored
    /// stat — field evidence: hostnet 17–21 ms one session, a physically impossible 4.4 ms the
    /// next, same wired host. The core re-syncs the estimate mid-stream (60 s + step detection);
    /// each call is an atomic load behind the FFI.
    private var clockOffset: () -> Int64 = { 0 }
    /// Signalled when the pump thread exits, so `stop()` can join it (bounded) before `decoder.reset()`
    /// — otherwise a pump iteration already past its `token.isStopped` check can rebuild a decode session
    /// right after the reset (a brief orphan session). `pumpJoinable` is armed by `start`, consumed by
    /// the first `stop` (so the idempotent second `stop`/deinit doesn't block on an already-drained
    /// semaphore). start/stop are sequential lifecycle calls, so the plain flag is safe.
    private let pumpStopped = DispatchSemaphore(value: 0)
    private var pumpJoinable = false

    /// Render-thread plumbing. `renderSignal` wakes the render thread — signalled by the DECODER
    /// callback on every frame (the primary trigger: presentation must never be gated on the
    /// display link, see the header) and by each display-link tick (the `putBack` retry + the
    /// vsync-clock refresh). Signals coalesce harmlessly (an extra wake finds an empty ring and
    /// goes back to sleep). `vsyncClock` is the link's last phase/period for V-Sync-mode
    /// scheduling. Lock-guarded boxes — the render thread, like the pump thread, must not capture
    /// `self`, or a missed stop() would leak a spinning pipeline. `renderStopped`/`renderJoinable`
    /// mirror the pump's bounded join.
    private let renderSignal = DispatchSemaphore(value: 0)
    private let vsyncClock = VsyncClock()
    private let renderStopped = DispatchSemaphore(value: 0)
    private var renderJoinable = false
    /// Deadline pacing's staged CAMetalDisplayLink frame-rate hint (see `FrameRateHint`).
    /// Created unconditionally (cheap); only the deadline link thread drains it.
    private let frameRateHint = FrameRateHint()
    /// The Direct gate's mode (`PresentPolicy.directGate`); idle on every other policy.
    private let scanout = ScanoutPacer()
    /// The pf-present line, on every pacing. Built in `init` so the decode callback can count
    /// decoder output; `start` names the pace once the session's policy is resolved.
    private let debugStats: PresentDebugStats

    /// The Metal layer the hosting view installs + sizes.
    public var layer: CAMetalLayer { presenter.layer }
    /// Deadline pacing found the layer wedged (see `LinkStallPolicy`): fires from the render
    /// thread, once per wedge. The owner's `WedgePolicy` picks the cure; the pipeline keeps a
    /// live link meanwhile and re-arms once a frame reaches glass. Set before `start`.
    public var onPresentWedged: (@Sendable () -> Void)?

    /// `endToEndMeter` records capture→on-glass per presented frame for the A/V sync loop; the
    /// overlay's stamps reach the core through the connection. Metering never gates the
    /// presenter choice. Returns nil if Metal can't be set up (headless / no GPU) — caller
    /// falls back to the stage-1 presenter. `pacing` also selects the decoded video sink when its
    /// `videoRenderer` is supplied. `gateDepth` bounds glass presents; `vsyncPaced` schedules
    /// macOS smoothness, while `adaptiveSlotPaced` schedules latency onto the ordinary
    /// display-link grid.
    public init?(
        endToEndMeter: LatencyMeter?,
        videoRenderer: AVSampleBufferVideoRenderer? = nil,
        pacing: PresentPacing = .arrival,
        gateDepth: Int = 1,
        storePolicy: FrameStorePolicy = .newestWins,
        vsyncPaced: Bool = false,
        adaptiveSlotPaced: Bool = false
    ) {
        let decodedSink: DecodedVideoSink?
        if pacing == .decoded {
            guard let videoRenderer else { return nil }
            decodedSink = DecodedVideoSink(renderer: videoRenderer)
        } else {
            decodedSink = nil
        }
        guard let presenter = MetalVideoPresenter.make() else { return nil }
        self.presenter = presenter
        self.pacing = pacing
        self.gateDepth = gateDepth
        self.vsyncPaced = vsyncPaced
        self.adaptiveSlotPaced = adaptiveSlotPaced
        // Deadline pacing under the latency intent keeps two, so `Carry` sees a collision.
        self.ring = FrameStore(
            policy: pacing == .deadline && storePolicy == .newestWins ? .newestTwo : storePolicy)
        self.endToEndMeter = endToEndMeter
        self.decodedSink = decodedSink
        switch storePolicy {
        case .newestWins, .newestTwo: self.cadence = nil
        case .fifo: self.cadence = CadenceClock(tuning: .snapping())
        }
        let ring = ring
        let recovery = recovery
        let renderSignal = renderSignal
        let decodeReport = decodeReport
        let frameHDR = frameHDR
        let hud = hud
        let cadence = cadence
        let rateHint = frameRateHint
        let vsyncClock = vsyncClock
        let scanout = scanout
        let paceName = { rateHint.pace() }
        let debugStats = PresentDebugStats(
            cadence: cadence, pace: paceName,
            linkPeriod: { vsyncClock.lastPeriod() }, panel: { rateHint.panel() },
            scanoutSwitches: { scanout.drainSwitches() })
        self.debugStats = debugStats
        self.decoder = VideoDecoder(
            onDecoded: { frame in
                // Decode stage = received→decoded, both client CLOCK_REALTIME (offset 0 — no
                // skew applies). Stamped at decode completion, so it covers every decoded frame,
                // including ones the core's freeze holds or the newest-wins ring drops.
                hud.decoded(
                    ptsNs: frame.ptsNs, receivedNs: frame.receivedNs, decodedNs: frame.decodedNs)
                debugStats.decoded(
                    isRepeat: frame.flags & PunktfunkConnection.userFlagRepeat != 0)
                // Same interval, reported to the core bitrate controller so Automatic caps at this
                // device's real decode limit instead of the network link ceiling. Every decoded
                // frame (not just presented ones), so a newest-wins drop can't hide the backlog.
                decodeReport.record(receivedNs: frame.receivedNs, decodedNs: frame.decodedNs)
                // VideoToolbox returns Ok with a gray picture for a delta whose reference is gone.
                // While the core's freeze holds, such a frame is not submitted, so the CAMetalLayer
                // keeps its last good drawable until a proven re-anchor lifts it.
                if recovery.decoded(flags: frame.flags) { return }
                if case .video(_, let isHDR) = frame.image { frameHDR.note(isHDR) }
                if let decodedSink {
                    let submitStarted = CACurrentMediaTime()
                    let submitted = decodedSink.submit(frame)
                    debugStats.renderReturned(
                        ok: submitted, tookMs: (CACurrentMediaTime() - submitStarted) * 1000)
                    return
                }
                // Decoder OUTPUT is where the cadence loop is sampled — the instant the frame
                // becomes presentable. Receipt would not model decode at all and could hand back a
                // due time already past by the moment the frame exists; dequeue would fold the
                // present path's own wait into the estimate and make the loop chase its output.
                ring.submit(Stage2Pipeline.dated(frame, by: cadence, hint: rateHint))
                // FRAME ARRIVAL is the render trigger (never the display link — see the header).
                renderSignal.signal()
            },
            // Async decode failure: reported to the core, which freezes and asks for an IDR after
            // three in a row. One WARN per run of refusals carries the OSStatus, the only trace
            // a field log has of what VideoToolbox refused.
            onDecodeError: { status in
                if recovery.refused() {
                    pumpLog.warning(
                        "video: VideoToolbox refused an AU status=\(status, privacy: .public)")
                }
            })
    }

    /// Start the AU pump, decoder, and selected presentation loop on the main thread.
    ///
    /// `onFrame` fires at receipt for host/network metering. `onDecodedSize` reports coded-size
    /// changes, `onFrameHDR` the decoded frames' HDR state as it changes, and `onSessionEnd`
    /// transport closure. Presentation records the live
    /// host-minus-client clock offset at each on-glass callback so end-to-end samples remain valid
    /// after clock resynchronization.
    ///
    /// A stopped pipeline is permanent; construct a new instance for another session. `screen` past
    /// 0 is a further screen's pipeline: it decodes that screen and leaves the session's loss
    /// recovery, bitrate signal, stats and lent AU memory to screen 0's.
    #if os(visionOS)
    /// Draw into the curved screen instead of the layer. Before `start`. Main thread.
    func setCurve(_ curve: CurveTarget?) {
        presenter.curve = curve
    }
    #endif

    public func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil,
        onFrameHDR: (@Sendable (Bool) -> Void)? = nil,
        screen: UInt8 = 0
    ) {
        clockOffset = { connection.clockOffsetNs } // live (re-synced) — see the field doc
        frameHDR.bind(onFrameHDR)
        if screen == 0 {
            recovery.bind(connection) // the decode callbacks' reports to the core's loss recovery
            decodeReport.bind(connection) // arm the Automatic-bitrate decode signal for this session
            hud.bind(connection) // the overlay's decode, display and floor stamps
            connection.recoveryReset() // a pipeline start is a fresh decoder: the freeze starts over
        }
        // A fresh session is a fresh source clock: re-anchor on its first frame rather than slew
        // for seconds off the previous host's offset. (Mid-session discontinuities — background
        // resume, a stream idle under the infinite GOP — arrive as a source-timestamp gap the loop
        // re-anchors on by itself; this seam covers the one it cannot see.)
        cadence?.reset()
        token = StopFlag() // fresh token per start — a stop is permanent (like StreamPump)

        // Configure the decoder's chroma + depth and the layer's initial colorimetry before the
        // first frame. The Welcome's HDR and depth seed the layer; a mid-session flip then
        // overrides per frame from the decoded buffer.
        decoder.setChroma444(connection.isChroma444)
        decoder.setCodec(connection.videoCodec)
        decoder.setBitDepth(connection.bitDepth)
        presenter.configure(hdr: connection.isHDR, tenBitSDR: connection.bitDepth >= 10)
        decodedSink?.reset()

        // PyroWave rides a different decode half: no CMFormatDescription/VideoToolbox machinery
        // (a wavelet AU has no parameter sets), no keyframe recovery or re-anchor freeze (the
        // stream is all-intra and Phase 4's partial delivery WANTS lossy frames on glass as
        // localized blur, not a freeze). The ready ring, render thread, pacing and meters are
        // shared unchanged.
        let presenter = presenter
        let thread =
            connection.videoCodec == .pyrowave
            ? Self.makePyroWavePump(
                connection: connection, token: token, pumpStopped: pumpStopped,
                ring: ring, renderSignal: renderSignal,
                device: presenter.metalDevice, queue: presenter.metalQueue,
                hud: hud, stats: debugStats, cadence: cadence, rateHint: frameRateHint,
                onFrame: onFrame, onSessionEnd: onSessionEnd, onDecodedSize: onDecodedSize,
                frameHDR: frameHDR,
                onHdrMeta: { [weak presenter] meta in presenter?.setHdrMeta(meta) },
                screen: screen)
            : Self.makeVideoToolboxPump(
                connection: connection, token: token, pumpStopped: pumpStopped,
                decoder: decoder,
                onFrame: onFrame, onSessionEnd: onSessionEnd, onDecodedSize: onDecodedSize,
                onHdrMeta: { presenter.setHdrMeta($0) }, screen: screen)
        thread.name = "punktfunk-stage2-pump"
        thread.qualityOfService = .userInteractive
        pumpJoinable = true
        thread.start()

        // Present policy, resolved once per session before the stats so each line names it.
        // Adaptive-refresh latency chooses immediate sparse or slotted dense input.
        let policy = PresentPolicy.resolve(
            env: ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENT_MODE"],
            vsync: connection.settings.vsync, vsyncPaced: vsyncPaced,
            adaptiveSlotPaced: adaptiveSlotPaced)
        frameRateHint.stagePace(policy.label(pacing))
        // The video plane has no present thread: `renderTick` stamps and flushes its line.
        let debugStats: PresentDebugStats? = self.debugStats
        if decodedSink != nil { return }

        // The present half. Deadline pacing (stage-4) swaps it wholesale: a CAMetalDisplayLink
        // vends the drawables and its per-refresh updates co-drive the render thread. The policy
        // above doesn't apply there (the link deadline-times every present).
        if pacing == .deadline {
            startDeadlinePresenter(debugStats: debugStats)
        } else {
            startArrivalPresenter(policy: policy, debugStats: debugStats)
        }
    }

    /// The render thread's take from the ring. Smoothness turns the FIFO into a holding buffer:
    /// a frame comes out once its cadence clock says it is due. Latency has no clock and takes
    /// whatever arrived.
    private func makeTakeReady() -> () -> ReadyFrame? {
        let ring = ring
        return cadence == nil
            ? { ring.take() }
            : { ring.take(dueBy: CACurrentMediaTime(), due: { $0.dueMediaTime }) }
    }

    /// Drawables in the deadline link's pool. `PUNKTFUNK_DRAWABLE_COUNT=2` is the A/B; a value
    /// outside 2...3 is ignored.
    nonisolated static func drawableCount(env: String?) -> Int {
        env.flatMap(Int.init).flatMap { (2...3).contains($0) ? $0 : nil } ?? 3
    }

    /// Arrival and glass pacing's present half (stage-2/3 — see `PresentPacing`): one render
    /// thread, one present per wake. Decoded frames are the primary wake; display-link ticks
    /// retry a put-back frame and refresh the vsync grid `policy` schedules against. Under
    /// `policy.directGate`, `ScanoutPacer` picks per frame between an immediate present and the
    /// adaptive slot, and every presented callback wakes the thread too. `ArrivalSlots` decides
    /// each wake's slot; the thread reads the clocks and gates and applies the plan.
    ///
    /// The thread owns every layer format/colour/drawable interaction (see MetalVideoPresenter's
    /// threading notes); with displaySyncEnabled on, nextDrawable's up-to-a-frame wait lands here
    /// instead of on main. The 100 ms timed wait is only the stop-flag poll for a session whose
    /// link stopped ticking. Like the pump, the thread never captures `self`.
    private func startArrivalPresenter(policy: PresentPolicy, debugStats: PresentDebugStats?) {
        let token = token
        let ring = ring
        // The render thread draws through it, as MetalVideoPresenter's threading note allows.
        nonisolated(unsafe) let presenter = presenter
        let vsyncClock = vsyncClock
        let endToEndMeter = endToEndMeter
        let hud = hud
        let clockOffset = clockOffset
        let renderSignal = renderSignal
        let renderStopped = renderStopped
        // Stage-3's bounded in-flight present gate; nil = stage-2's present-on-arrival. A local
        // (like the ring) so neither the render thread nor the presented handlers capture `self`.
        let gate: PresentGate? = pacing == .glass ? PresentGate(capacity: gateDepth) : nil
        // The adaptive panel's Direct gate: immediate presents while the layer scans out Direct.
        let scanout: ScanoutPacer? = policy.directGate ? self.scanout : nil
        let rateHint = frameRateHint
        let takeReady = makeTakeReady()
        let renderThread = Thread {
            defer { renderStopped.signal() }
            var slots = ArrivalSlots(policy: policy)
            let nextVsync = { vsyncClock.nextVsync(after: $0) }
            let gatedWake = {
                debugStats?.gatedWake()
                debugStats?.flushIfDue(ring: ring, gate: gate)
            }
            // Every iteration drains its own autorelease pool (`return` = the old `continue`):
            // this thread has no runloop, and `nextDrawable()` AUTORELEASES each CAMetalDrawable —
            // without a per-iteration pool every presented frame's drawable object (plus its
            // texture-descriptor/array retinue, ~2 MB/min at 120 fps) piles up until session end.
            while !token.isStopped { autoreleasepool {
                if renderSignal.wait(timeout: .now() + .milliseconds(100)) == .timedOut {
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                // Fixed slot pacing can reject the wake before touching the store. Adaptive pacing
                // resolves after taking a frame because its source stamp selects the regime.
                let fixed = slots.fixedSlot(now: CACurrentMediaTime(), nextVsync: nextVsync)
                if fixed.taken {
                    gatedWake()
                    return
                }
                // Stage-3: while a present is in flight, don't take from the ring at all — frames
                // keep coalescing there (newest wins, the intended drop point) and the presented
                // handler re-signals the moment the slot frees. Checked BEFORE the take so a gated
                // frame is never bounced through putBack.
                if let gate, !gate.tryAcquire(now: CACurrentMediaTime()) {
                    gatedWake()
                    return
                }
                // Direct: likewise wait until the previous present has flipped; the callback
                // re-signals. The glass gate, if any, was acquired above and is let go here, or
                // it would hold its slot until a present that never comes.
                let refresh = ArrivalSlots.refresh(panelInterval: rateHint.panel().minInterval)
                let direct = scanout?.mode(now: CACurrentMediaTime()) == .direct
                if direct, let scanout,
                   !scanout.mayIssue(now: CACurrentMediaTime(), minInterval: refresh) {
                    gate?.release()
                    gatedWake()
                    return
                }
                guard !token.isStopped, let frame = takeReady() else {
                    gate?.release() // armed but nothing to render — don't hold the gate stale
                    debugStats?.emptyWake()
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                let plan = slots.plan(
                    ptsNs: frame.ptsNs, dueMediaTime: frame.dueMediaTime,
                    now: CACurrentMediaTime(), fixedTarget: fixed.target, direct: direct,
                    refresh: refresh, nextVsync: nextVsync)
                guard case .present(let presentAt, let slotTarget, let holdUnit) = plan else {
                    gate?.release()
                    ring.putBack(frame)
                    gatedWake()
                    return
                }
                let renderStarted = CACurrentMediaTime()
                let issuedNs = Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: renderStarted)
                scanout?.issued(at: renderStarted, direct: direct)
                let onGlass: (Int64?) -> Void = { presentedNs in
                    // Stage-3: the flip reached glass (or was dropped) — free the present slot,
                    // then re-signal so the freshest waiting ring frame goes out immediately.
                    if let gate {
                        gate.release()
                        renderSignal.signal()
                    }
                    if let scanout {
                        let calledAt = CACurrentMediaTime()
                        scanout.landed(issuedAt: renderStarted)
                        renderSignal.signal()
                        if direct {
                            scanout.noteDirect(
                                hold: calledAt - renderStarted, interval: holdUnit, now: calledAt)
                        }
                    }
                    Stage2Pipeline.stampPresented(
                        frame, presentedNs: presentedNs, issuedNs: issuedNs,
                        endToEnd: endToEndMeter, clockOffset: clockOffset, hud: hud,
                        stats: debugStats)
                }
                let rendered = presenter.render(
                    frame.image, presentAtMediaTime: presentAt, onPresented: onGlass)
                debugStats?.renderReturned(
                    ok: rendered, tookMs: (CACurrentMediaTime() - renderStarted) * 1000)
                if !rendered {
                    gate?.release() // no present registered — its handler will never fire
                    scanout?.landed(issuedAt: renderStarted)
                    ring.putBack(frame)
                } else {
                    slots.presented(slot: slotTarget)
                    if direct { debugStats?.directPresent() }
                }
                debugStats?.flushIfDue(ring: ring, gate: gate)
            } }
        }
        renderThread.name = "punktfunk-stage2-render"
        renderThread.qualityOfService = .userInteractive
        renderJoinable = true
        renderThread.start()
    }

    /// Deadline pacing's present half (stage-4 — see `PresentPacing.deadline`): a
    /// CAMetalDisplayLink on its own runloop thread vends one drawable per refresh, two refreshes
    /// before glass, into the newest-wins stash. The render thread pairs it with the newest
    /// decoded frame and presents by the drawable's deadline (`DeadlinePacer`). A drawable no
    /// frame claims by then goes back to the pool at once: the three-drawable pool has no slot to
    /// spare, and a held one would cost the next vend. `LinkWatchdog` judges a silent link and
    /// `DeadlineSelector` the pick and the carry; the thread applies their answers.
    ///
    /// Threading mirrors the arrival/glass half: neither thread captures `self`; the link is
    /// created, driven and invalidated entirely on its own thread (CAMetalDisplayLink is only
    /// ever touched there — the frame-rate hint crosses via `FrameRateHint`); the link thread's
    /// runloop iterations each drain an autorelease pool (a vended CAMetalDrawable is
    /// autoreleased like a `nextDrawable()` one — see the render loop's identical rule); the
    /// 100 ms runloop horizon is the stop-flag poll, so teardown is bounded without a join.
    private func startDeadlinePresenter(debugStats: PresentDebugStats?) {
        let token = token
        let ring = ring
        let renderSignal = renderSignal
        let renderStopped = renderStopped
        // The render and link threads draw through both, as MetalVideoPresenter's threading
        // note allows: main touches only the layer's geometry.
        nonisolated(unsafe) let presenter = presenter
        let endToEndMeter = endToEndMeter
        let hud = hud
        let clockOffset = clockOffset
        let hint = frameRateHint
        nonisolated(unsafe) let layer = presenter.layer
        let onWedged = onPresentWedged
        let stash = LatestBox<VendedDrawable>()
        // Set by a presented handler the system stamped: the watchdog's re-arm signal.
        let glassSeen = LatestBox<Bool>()
        // Under cadence targeting the link's vend IS the grid snap: the clock only holds a frame
        // back until it is due, and the next update presents it.
        let takeReady = makeTakeReady()

        // Three drawables: the link vends two refreshes before glass, so one scans out, one waits
        // for its flip and one is ours. Two halve a 120 Hz panel. Set before either thread exists;
        // afterwards the link thread applies layer writes (`deferLayerWrites`).
        let drawableCount = Self.drawableCount(
            env: ProcessInfo.processInfo.environment["PUNKTFUNK_DRAWABLE_COUNT"])
        layer.maximumDrawableCount = drawableCount

        // The latency intent picks inside the window before the deadline; Smoothness keeps its
        // cadence clock's timing and presents on pairing.
        let pacer = DeadlinePacer()
        let latePick = cadence == nil

        // The link starts LAZILY — the render thread triggers this after the FIRST decoded
        // frame's reconcileLayer. Started eagerly it vends into the layer's initial 0×0
        // drawableSize for the whole connect window: every vend fails allocation and the system
        // logs "[CAMetalLayer nextDrawable] returning nil because allocation failed" once per
        // refresh until the first frame arrives. Before that frame there is nothing to present
        // anyway, and the first frame waits at most one refresh for the first vend.
        // Per-GENERATION stop flag (the session `token` still stops every generation): the
        // watchdog below retires a link that stopped vending and starts a fresh one, and the
        // retired thread must exit without taking the session with it.
        let startLink: (StopFlag) -> Void = { linkStop in
            let linkThread = Thread {
                let delegate = DeadlineLinkDelegate(
                    presenter: presenter,
                    stash: stash, renderSignal: renderSignal, hint: hint, stats: debugStats,
                    hud: hud,
                    drawableCount: drawableCount, pacer: pacer, latePick: latePick)
                let link = CAMetalDisplayLink(metalLayer: layer)
                link.preferredFrameLatency = DeadlineLinkDelegate.frameLatency
                if let range = hint.drain() ?? hint.current() { link.preferredFrameRateRange = range }
                // The link holds the delegate WEAKLY and `delegate` is a local, not a capture —
                // its last use is this store, so ARC may release it right here and leave a link
                // that never calls back. `withExtendedLifetime` is the strong ref, not the
                // closure (which captures only the values the init consumed).
                link.delegate = delegate
                link.add(to: RunLoop.current, forMode: .default)
                withExtendedLifetime(delegate) {
                    while !token.isStopped, !linkStop.isStopped {
                        autoreleasepool {
                            _ = RunLoop.current.run(
                                mode: .default, before: Date(timeIntervalSinceNow: 0.1))
                        }
                    }
                }
                link.invalidate()
            }
            linkThread.name = "punktfunk-stage4-link"
            linkThread.qualityOfService = .userInteractive
            linkThread.start()
        }

        let renderThread = Thread {
            defer { renderStopped.signal() }
            let started = CACurrentMediaTime()
            var lastDeadlineLine = started
            var watchdog = LinkWatchdog(now: started)
            var selector = DeadlineSelector(latePick: latePick, now: started)
            // The live link generation's stop flag — nil until the first frame starts one.
            // Render-thread confined (only this thread starts, retires or reads it).
            var linkStop: StopFlag?
            let restartLink = {
                linkStop?.stop()
                let stop = StopFlag()
                linkStop = stop
                startLink(stop)
            }
            // What became of the last drawable this thread handled, for the late line.
            var lastFate = "presented"
            // The newest vend's pass, parked on the GPU until its pick (MetalVideoPresenter.park),
            // while the gate says the vends are fed.
            var parked: ParkedPass?
            var latchGate = LatchGate()
            // Per-iteration autorelease pool — same contract as the arrival/glass loop (the
            // vended drawable and its retinue are autoreleased objects on a runloop-less thread).
            while !token.isStopped { autoreleasepool {
                // Wake on a signal, or when the stashed drawable's pick passes: one no frame
                // claimed goes back to the pool then.
                // PUNKTFUNK_DEADLINE_SLACK_MS (DEBUG probe): pick that much past the margin,
                // into the slack the display keeps after Apple's deadline; `late` says how much
                // there is.
                let margin = pacer.margin - Stage2Pipeline.deadlineSlack
                let cutoff = { (v: VendedDrawable) in
                    DeadlinePacer.cutoff(vendAt: v.vendAt, deadline: v.deadline, margin: margin)
                }
                let untilPick = stash.peek { cutoff($0) - CACurrentMediaTime() } ?? 0.1
                let signaled =
                    renderSignal.wait(timeout: .now() + min(max(untilPick, 0), 0.1)) == .success
                // Late latch: the newest vend's pass goes to the GPU now, and an older one it
                // replaced runs as a discard; the pick then only names the frame.
                if let vend = stash.peek({ $0 }), parked?.drawable !== vend.drawable {
                    if let old = parked {
                        presenter.skip(old)
                        latchGate.note(shown: false)
                    }
                    parked = latchGate.open ? presenter.park(vend.drawable) : nil
                }
                if let vend = stash.take(where: { CACurrentMediaTime() > cutoff($0) }) {
                    if let p = parked, p.drawable === vend.drawable {
                        presenter.skip(p)
                        parked = nil
                    }
                    latchGate.note(shown: false)
                    watchdog.noteVend(at: CACurrentMediaTime()) // the link is alive, just unclaimed
                    pacer.noteExpired()
                    lastFate = "expired"
                }
                // Every second to stdout under PUNKTFUNK_PRESENT_DEBUG; otherwise only a window
                // with a late or dropped present, so a log bundle carries the GPU cost and margin.
                if CACurrentMediaTime() - lastDeadlineLine >= 1 {
                    lastDeadlineLine = CACurrentMediaTime()
                    let window = pacer.windowLine()
                    if let tail = selector.endWindow(
                        report: presentDebug || window.troubled,
                        composition: { Stage2Pipeline.compositionStatus(layer) })
                    {
                        let headroom = presenter.stagedHeadroom
                        Stage2Pipeline.emitDeadlineLine(
                            window.text + tail
                                + String(
                                    format: " headroom=%.2f potential=%.2f hdrOut=%d",
                                    headroom.current, headroom.potential,
                                    headroom.hdrOutput ? 1 : 0))
                    }
                }
                guard signaled else {
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                }
                // Present needs the PAIR — frame first. The frame drives the layer reconcile,
                // which must run even when NO drawable is vended yet: the link vends from the
                // layer's CURRENT config, so drawableSize/format have to be right before a vend
                // can succeed at all (see reconcileLayer — the session-start bootstrap, where
                // the layer still has its initial 0×0 size and every vend fails allocation).
                guard !token.isStopped, let ready = takeReady() else {
                    debugStats?.emptyWake()
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                }
                var frame = ready
                presenter.reconcileLayer(for: frame.image)
                // First frame: the layer now has a real config — start vending (see startLink).
                // From here the link thread vends, so it also applies every layer write.
                if linkStop == nil {
                    presenter.deferLayerWrites()
                    watchdog.noteVend(at: CACurrentMediaTime())
                    restartLink()
                }
                guard let vended = stash.take() else {
                    // No vend yet (session start: the reconcile above just unblocked the
                    // allocator, the link's next update delivers; steady state: decode beat the
                    // link's phase). putBack keeps newest-wins — a fresher decode replaces this
                    // frame while it waits, and the update's signal retries the pairing.
                    ring.putBack(frame)
                    debugStats?.noDrawableWake()
                    // …unless the link has gone silent. Only a decoded frame reaches here, so a
                    // quiet stream never trips the watchdog.
                    switch watchdog.noVend(
                        linkRunning: linkStop != nil, canReport: onWedged != nil,
                        now: CACurrentMediaTime())
                    {
                    case .none:
                        break
                    case .relink(let ms):
                        restartLink()
                        presentLog.error(
                            "stage4: link stalled \(ms) ms with no vend — retiring it, relinking")
                    case .wedged(let ms):
                        restartLink()
                        _ = glassSeen.take() // only glass AFTER the wedge re-arms
                        presentLog.error(
                            "stage4: link stalled \(ms) ms again right after a relink — layer wedged")
                        onWedged?()
                    }
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                }
                // A vend is not glass: a wedged layer still vends its pool once. Only a frame
                // the system showed re-arms the watchdog.
                if watchdog.rearm(glass: { glassSeen.take() == true }) {
                    presentLog.info("stage4: the display takes frames again — watchdog re-armed")
                }
                let drawable = vended.drawable
                let now = CACurrentMediaTime()
                let drain = pacer.takeDrain(
                    nowNs: Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: now))
                let last = cutoff(vended)
                var held = false, carried = false
                switch selector.pair(
                    vendAt: vended.vendAt, period: vended.period, last: last, now: now,
                    drain: drain)
                {
                case .skip:
                    // Past the pick, or draining a late present: the frame waits for the next
                    // vend, and this drawable goes back to the pool unpresented.
                    watchdog.noteVend(at: now)
                    ring.putBack(frame)
                    pacer.noteSkip()
                    latchGate.note(shown: false)
                    if let p = parked, p.drawable === drawable {
                        presenter.skip(p)
                        parked = nil
                    }
                    lastFate = "skipped"
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                case .present(let at, let switched):
                    if latePick {
                        (frame, held, carried) = Stage2Pipeline.holdForPick(
                            frame, at: at, switched: switched, period: vended.period,
                            selector: &selector, ring: ring, pacer: pacer)
                    }
                }
                let shown = frame
                selector.noteShown(decodedNs: shown.decodedNs, period: vended.period) {
                    Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: vended.vendAt)
                }
                let targetNs = Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: vended.target)
                let cutoffLeadNs = Int64((vended.target - last) * 1e9)
                let renderStarted = CACurrentMediaTime()
                watchdog.noteVend(at: renderStarted)
                let issuedNs = Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: renderStarted)
                // This present's GPU span: the presenter reads the hook as the frame encodes.
                let gpuSpan = LatestBox<(start: CFTimeInterval, end: CFTimeInterval)>()
                let deadline = vended.deadline, vendAt = vended.vendAt
                presenter.onGPUTime = { start, end in
                    pacer.noteGPU(
                        issuedAt: renderStarted, start: start, end: end, deadline: deadline)
                    gpuSpan.put((start: start, end: end))
                }
                let after = lastFate, inFlight = pacer.inFlight
                let onGlass: (Int64?) -> Void = { [held, carried] presentedNs in
                    if presentedNs != nil { glassSeen.put(true) }
                    let lateCount = pacer.observe(
                        issuedNs: issuedNs, presentedNs: presentedNs, targetNs: targetNs,
                        cutoffLeadNs: cutoffLeadNs)
                    // Every late present under PUNKTFUNK_PRESENT_DEBUG, else four a window line.
                    if lateCount > 0, presentDebug || lateCount <= 4, let presentedNs {
                        Stage2Pipeline.emitDeadlineLine(LatePresent(
                            lateNs: presentedNs - targetNs, vendAt: vendAt,
                            issuedAt: renderStarted, deadline: deadline,
                            gpu: gpuSpan.peek { $0 }, held: held, carried: carried,
                            after: after, inFlight: inFlight,
                            decodeToIssueNs: shown.decodedNs > 0
                                ? issuedNs - shown.decodedNs : nil
                        ).line)
                    }
                    Stage2Pipeline.stampPresented(
                        shown, presentedNs: presentedNs, issuedNs: issuedNs,
                        endToEnd: endToEndMeter, clockOffset: clockOffset, hud: hud,
                        stats: debugStats)
                }
                // A parked pass takes the frame by name; one it cannot take is skipped and the
                // frame retries on the next vend, as a failed render does.
                let rendered: Bool
                if let p = parked, p.drawable === drawable {
                    parked = nil
                    if case .video(let buffer, let isHDR) = shown.image {
                        rendered = presenter.latch(p, buffer, isHDR: isHDR, onPresented: onGlass)
                    } else {
                        presenter.skip(p)
                        rendered = false
                    }
                } else {
                    rendered = presenter.render(shown.image, into: drawable, onPresented: onGlass)
                }
                lastFate = rendered ? "presented" : "failed"
                latchGate.note(shown: rendered)
                let renderTook = CACurrentMediaTime() - renderStarted
                pacer.noteRender(seconds: renderTook)
                debugStats?.renderReturned(ok: rendered, tookMs: renderTook * 1000)
                if !rendered {
                    // The vended drawable is spent either way (an unused/mismatched one drops
                    // back to the pool); the frame retries on the link's next vend. A format
                    // mismatch (mid-session HDR flip caught between the layer reconfigure and
                    // the next vend) self-heals the same way — see encodePresent's guard.
                    ring.putBack(shown)
                } else {
                    pacer.noteIssued(ns: issuedNs)
                }
                debugStats?.flushIfDue(ring: ring, gate: nil)
            } }
            presenter.flushParked()
        }
        renderThread.name = "punktfunk-stage2-render"
        renderThread.qualityOfService = .userInteractive
        renderJoinable = true
        renderThread.start()
    }

    /// Consume an ordinary display-link tick on the main thread.
    ///
    /// The target refresh updates scheduled-present timing. For decoded-video presentation, the
    /// currently displayed IOSurface is correlated to its frame and stamped at this tick's
    /// just-finished refresh. Other pacings signal the render thread only as a retry;
    /// decoded-frame arrival remains their primary trigger. Deadline pacing has its own
    /// CAMetalDisplayLink and never calls this method.
    public func renderTick(
        targetMediaTime: CFTimeInterval, displayedMediaTime: CFTimeInterval,
        period: CFTimeInterval
    ) {
        vsyncClock.set(target: targetMediaTime, period: period)
        #if !os(macOS)
        if let stamp = decodedSink?.takeDisplayedStamp() {
            let atNs = Self.realtimeNs(forDisplayLinkTimestamp: displayedMediaTime)
            endToEndMeter?.record(ptsNs: stamp.ptsNs, atNs: atNs, offsetNs: clockOffset())
            hud.displayed(ptsNs: stamp.ptsNs, decodedNs: stamp.decodedNs, atNs: atNs)
            debugStats.presented(
                atNs: atNs, issuedNs: stamp.submittedNs, ptsNs: stamp.ptsNs,
                decodedNs: stamp.decodedNs, isRepeat: stamp.isRepeat)
        }
        if presentDebug, let decodedSink, displayedMediaTime - lastPlaneLine >= 1 {
            lastPlaneLine = displayedMediaTime
            decodedSink.logPerformance()
        }
        #endif
        if pacing != .decoded {
            renderSignal.signal()
        } else {
            debugStats.flushIfDue(ring: ring, gate: nil)
        }
    }

    /// The layer's private compositing status for the debug line: 0 scans out Direct, non-zero
    /// composites. Debug builds only; `na` where the property is missing.
    nonisolated static func compositionStatus(_ layer: CAMetalLayer) -> String {
        #if DEBUG
        let key = "displayCompositingInternalStatus"
        guard layer.responds(to: NSSelectorFromString(key)),
              let n = layer.value(forKey: key) as? NSNumber
        else { return "na" }
        return n.uint64Value == 0 ? "direct" : String(format: "0x%llx", n.uint64Value)
        #else
        return "na"
        #endif
    }

    /// MAIN thread: the hosting panel, for the pf-present line's grid and `panel=` field.
    public func setPanel(_ info: PanelInfo) {
        frameRateHint.stagePanel(info)
    }

    /// MAIN thread (SessionPresenter — session start + every layout/Reconfigure): hint the
    /// deadline link with the stream cadence. Staged; the link's own thread applies it (see
    /// `FrameRateHint`). Under arrival/glass pacing no link reads it — the hosting view's
    /// CADisplayLink is the hinted one there — but the stored rate is still the cadence clock's
    /// nominal source interval, and hence its cushion ceiling, on every pacing.
    public func setFrameRateHint(hz: Float) {
        frameRateHint.stage(hz: hz)
    }

    /// Pen-proximity rate boost (drawing workloads): drive the deadline link — and with it the
    /// panel, whose cadence paces UIKit's touch/Pencil event delivery — at the range ceiling
    /// while a Pencil is in range, so a sub-panel-rate stream stops halving pencil sampling.
    /// Staged like the rate hint; no-op under arrival/glass pacing. MAIN thread.
    public func setInteractionBoost(_ on: Bool) {
        frameRateHint.setBoost(on)
        presentLog.info("pen boost \(on ? "engaged" : "released", privacy: .public)")
    }

    /// Whether the window can scan out Direct — fullscreen, nothing of the app's own over the
    /// video; the Direct gate probes only then (MAIN).
    public func setDirectPossible(_ possible: Bool) {
        scanout.setDirectPossible(possible)
    }

    /// Forward the layout-derived drawable pixel size to the presenter (MAIN thread — see
    /// `MetalVideoPresenter.setDrawableTarget`).
    public func setDrawableTarget(_ size: CGSize) {
        presenter.setDrawableTarget(size)
    }

    /// Forward the visible part of the frame (MAIN thread — see
    /// `MetalVideoPresenter.setSourceRect`).
    public func setSourceRect(_ rect: CGRect) {
        presenter.setSourceRect(rect)
    }

    /// Forward the display's current EDR headroom to the presenter (MAIN thread — a `UIScreen`
    /// read): tvOS picks PQ passthrough or the in-shader tone-map on it, iOS rolls linear HDR
    /// highlights onto it. Both values show on the deadline window line. See
    /// `MetalVideoPresenter.setDisplayHeadroom`.
    public func setDisplayHeadroom(_ headroom: CGFloat, potential: CGFloat) {
        presenter.setDisplayHeadroom(headroom, potential: potential)
    }

    /// tvOS: the HDMI output switched into the HDR mode the session asked for (MAIN thread).
    /// HDR frames present as PQ passthrough from the next frame. See
    /// `MetalVideoPresenter.setHDROutput`.
    public func setHDROutput(_ on: Bool) {
        presenter.setHDROutput(on)
    }

    /// Stop the pump + render thread (≤ one poll timeout each) and drop the decode session. MAIN
    /// THREAD; idempotent. Does not close the connection. A restart needs a fresh Stage2Pipeline
    /// (the stop is permanent).
    public func stop() {
        token.stop()
        // Join the pump (bounded: ≤ one nextAU poll + an in-flight decode) before resetting the decoder,
        // so the pump can't rebuild a session right after the reset. Only the first stop joins; a
        // repeat/deinit stop skips the already-drained semaphore.
        if pumpJoinable {
            pumpJoinable = false
            _ = pumpStopped.wait(timeout: .now() + 0.5)
        }
        // Wake + join the render thread (bounded: it may sit in `nextDrawable` for up to ~a frame; a
        // timed-out join is fine — the loop exits at its next stop-flag check, and a final present on
        // the detached layer is harmless).
        if renderJoinable {
            renderJoinable = false
            renderSignal.signal()
            _ = renderStopped.wait(timeout: .now() + 0.5)
        }
        decoder.reset()
        recovery.bind(nil) // a late decode callback reports nothing once the session is torn down
    }

    deinit {
        token.stop()
        renderSignal.signal() // wake the render thread so it can observe the stop and exit
    }

    /// The VideoToolbox pump: the shared AU intake vets each AU and `decoder` decodes it; the
    /// decoder's own callback fills the ready ring. A failed submit drops the decode session and
    /// waits for the next IDR's parameter sets. Static + capture-by-parameter so a missed stop
    /// can't leak a live pipeline.
    private nonisolated static func makeVideoToolboxPump(
        connection: PunktfunkConnection, token: StopFlag, pumpStopped: DispatchSemaphore,
        decoder: VideoDecoder,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)?,
        onHdrMeta: @escaping @Sendable (PunktfunkConnection.HdrMeta) -> Void,
        screen: UInt8
    ) -> Thread {
        Thread {
            defer { pumpStopped.signal() } // let stop() join the pump (bounded) before decoder.reset()
            var intake = AUIntake(connection: connection, screen: screen)
            // The core assembles each of screen 0's AUs in a pool slot, and the sample is that
            // slot. A further screen's arrive as copies and need none.
            let lends = screen == 0
            var pool = AccessUnitSlotPool(capacity: AccessUnitSlotPool.defaultCapacity)
            if lends {
                decoder.accessUnitMemory = pool
                connection.setAccessUnitMemory(pool)
            }
            defer { if lends { connection.setAccessUnitMemory(nil) } }
            // Hardware-only backstop (4:4:4, AV1): 3 s in which every decode fails means this device
            // can't decode the mode at all, e.g. past a resolution ceiling. End instead of looping on
            // black. Timed, not counted: after a failure only IDRs reach the decoder.
            var hardwareOnly = connection.isChroma444 || connection.videoCodec == .av1
            var failingSinceNs: UInt64?
            // Every iteration drains its own autorelease pool: this thread has no runloop, so
            // autoreleased VT/CM temporaries would otherwise accumulate until session end.
            // `false` = session over — exit the loop (the closure can't `break` across itself).
            var alive = true
            while alive, !token.isStopped {
                alive = autoreleasepool { () -> Bool in
                    do {
                        // Background time is not failure time: the intake drains without decoding.
                        if connection.isVideoDropped { failingSinceNs = nil }
                        // HDR mastering metadata (0xCE) goes to the presenter (→ CAEDRMetadata).
                        guard let ready = try intake.next(
                            onFrame: onFrame, onDecodedSize: onDecodedSize, onHdrMeta: onHdrMeta)
                        else { return true }
                        let au = ready.au
                        if ready.newFormat {
                            // Built for the format the connection reads now; the session the
                            // decoder held was the old one's.
                            decoder.setChroma444(connection.isChroma444)
                            decoder.setCodec(connection.videoCodec)
                            decoder.setBitDepth(connection.bitDepth)
                            decoder.reset()
                            hardwareOnly = connection.isChroma444 || connection.videoCodec == .av1
                            failingSinceNs = nil
                        }
                        // A delta between a loss and its re-anchor references the lost picture. Fed
                        // to VideoToolbox it poisons the session — every later non-IDR AU, the anchor
                        // too, comes back kVTVideoDecoderBadDataErr. Withheld, the anchor decodes.
                        if ready.step.withhold { return true }
                        guard let f = intake.pump.format, !token.isStopped else { return true }
                        if lends {
                            pool = AccessUnitSlotPool.grown(
                                for: au, keyframe: ready.idr, from: pool, connection: connection)
                            decoder.accessUnitMemory = pool
                        }
                        if decoder.decode(au: au, format: f) {
                            failingSinceNs = nil
                        } else {
                            // Submit/decoder error: drop the session and re-gate on the next IDR's
                            // in-band parameter sets (a delta frame can't recover) and keep asking.
                            decoder.reset()
                            intake.requireIDR()
                            let nowNs = DispatchTime.now().uptimeNanoseconds
                            let sinceNs = failingSinceNs ?? nowNs
                            failingSinceNs = sinceNs
                            if hardwareOnly, nowNs - sinceNs >= 3_000_000_000 {
                                if !token.isStopped { onSessionEnd?() }
                                return false
                            }
                        }
                        return true
                    } catch {
                        if !token.isStopped { onSessionEnd?() }
                        return false // session closed
                    }
                }
            }
        }
    }

    /// The PyroWave pump: AUs go straight into the Metal wavelet decoder (no VideoToolbox, no
    /// format descriptions), decoded planes ride the same ready ring / render thread. All-intra
    /// stream, so none of the VT pump's recovery machinery applies: keyframe/RFI requests are
    /// silenced host-side for this codec, and a lossy (partial-delivery) frame is MEANT to
    /// present as localized blur — never a freeze. Static + capture-by-parameter for the same
    /// reason the VT pump avoids capturing `self` (a missed stop must not leak a live pipeline).
    private nonisolated static func makePyroWavePump(
        connection: PunktfunkConnection, token: StopFlag, pumpStopped: DispatchSemaphore,
        ring: FrameStore<ReadyFrame>, renderSignal: DispatchSemaphore,
        device: MTLDevice, queue: MTLCommandQueue,
        hud: HudSink, stats: PresentDebugStats, cadence: CadenceClock?, rateHint: FrameRateHint,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)?,
        frameHDR: FrameHDRReporter,
        onHdrMeta: (@Sendable (PunktfunkConnection.HdrMeta) -> Void)?,
        screen: UInt8
    ) -> Thread {
        // The chunk-aligned parse window = the session's negotiated shard payload (Welcome);
        // the 64-byte floor mirrors the Rust client's guard against a nonsense value.
        let windowSize = max(64, Int(connection.shardPayload))
        return Thread {
            defer { pumpStopped.signal() }
            // Screen 0 lends the core its AU memory and drains the HDR plane; a further screen
            // pulls its own copies.
            let lends = screen == 0
            func pull() throws -> AccessUnit? {
                lends
                    ? try connection.nextAU(timeoutMs: 100)
                    : try connection.nextScreenAU(screen: screen, timeoutMs: 100)
            }
            // The core assembles each AU in a pool slot the dequant kernel reads as is: no copy
            // between the wire and the GPU. Sized for the mode; a bigger AU brings a bigger pool.
            let mode = connection.currentMode()
            let firstPool = WaveletAccessUnitPool(
                device: device,
                capacity: WaveletAccessUnitPool.capacity(
                    width: Int(mode.width), height: Int(mode.height),
                    chroma444: connection.isChroma444))
            // Compiles the two compute kernels on the session's first frames' thread — ~tens of
            // ms, once per session. Failure = this device can't run the negotiated codec (the
            // advertisement probe should have prevented this); end the session cleanly.
            // Ring past the store: its queued frames, the render thread's, a put-back, the decode.
            guard let firstPool, let decoder = MetalWaveletDecoder(
                device: device, queue: queue, tenBit: connection.bitDepth >= 10,
                ringDepth: max(4, ring.capacity + 3), pool: firstPool)
            else {
                if !token.isStopped { onSessionEnd?() }
                return
            }
            var pool = firstPool
            if lends { connection.setAccessUnitMemory(pool) }
            defer { if lends { connection.setAccessUnitMemory(nil) } }
            // Newest decoded frame index — a late partial (the reassembler's 30 ms fuse can
            // deliver one behind a newer complete frame) must not travel back in time.
            var newestIndex: UInt32?
            var lastDims: (w: Int, h: Int)?
            var alive = true
            while alive, !token.isStopped {
                alive = autoreleasepool { () -> Bool in
                    do {
                        // Background keep-alive: drain + discard before the Metal wavelet decode
                        // (PyroWave is all-intra, so the resumed frame heals on its own — no IDR
                        // request needed, just no GPU work off-screen).
                        if connection.isVideoDropped {
                            _ = try pull()
                            return true
                        }
                        // Mastering metadata (0xCE), same as the VideoToolbox pump: a PQ PyroWave
                        // session drives the same EDR machinery, and without this it tone-maps
                        // against the bare reference-white anchor with no mastering volume or
                        // content-light level for the whole session.
                        if lends, let meta = try? connection.nextHdrMeta(timeoutMs: 0) {
                            onHdrMeta?(meta)
                        }
                        guard let au = try pull() else { return true }
                        onFrame?(au)
                        if let newest = newestIndex,
                           Int32(bitPattern: au.frameIndex &- newest) <= 0 {
                            return true // stale (or duplicate) frame — skip
                        }
                        guard !token.isStopped else { return true }
                        let chunkAligned =
                            au.flags & PunktfunkConnection.userFlagChunkAligned != 0
                        let ptsNs = au.ptsNs
                        // Decode stage starts at the PULL (matching the VT path's FrameContext —
                        // receipt→pull is the HUD's separate client-queue term, ABI v9 split).
                        let receivedNs = au.pulledNs
                        let flags = au.flags
                        // In the pool already, or copied into a slot: an AU the core assembled
                        // on the heap, or one a mode switch made too big for this pool — which
                        // then gives way to one that fits, from the next frame on.
                        var placed = au.inPlace ?? pool.stage(au.data)
                        if placed == nil, au.byteCount > pool.capacity,
                           let bigger = WaveletAccessUnitPool(
                               device: device, capacity: au.byteCount * 5 / 4) {
                            pool = bigger
                            decoder.pool = bigger
                            if lends { connection.setAccessUnitMemory(bigger) }
                            placed = bigger.stage(au.data)
                        }
                        guard let placed else { return true } // no slot free: skip the frame
                        let submitted = decoder.decode(
                            placed, chunkAligned: chunkAligned, windowSize: windowSize
                        ) { planes in
                            // Metal completed-handler thread — stamp + enqueue, don't block
                            // (the exact contract of the VT output callback).
                            guard let planes else { return }
                            let decodedNs = realtimeNowNs()
                            hud.decoded(
                                ptsNs: ptsNs, receivedNs: receivedNs, decodedNs: decodedNs)
                            stats.decoded(
                                isRepeat: flags & PunktfunkConnection.userFlagRepeat != 0)
                            frameHDR.note(planes.pq)
                            // Same cadence sample as the VideoToolbox half: the wavelet decode's
                            // completion IS this frame's presentable instant.
                            ring.submit(
                                Stage2Pipeline.dated(
                                    ReadyFrame(
                                        ptsNs: ptsNs, receivedNs: receivedNs,
                                        decodedNs: decodedNs, image: .planar(planes),
                                        flags: flags),
                                    by: cadence, hint: rateHint))
                            renderSignal.signal()
                        }
                        if submitted {
                            newestIndex = au.frameIndex
                            // Decoded-size changes come from the SOF dims (this is also how a
                            // mid-stream Reconfigure lands here) — report like the VT pump.
                            if let size = decoder.decodedSize,
                               lastDims?.w != size.width || lastDims?.h != size.height {
                                lastDims = (size.width, size.height)
                                onDecodedSize?(size.width, size.height)
                            }
                        }
                        // A dropped AU (malformed / SOF lost / too few blocks) is just skipped:
                        // every PyroWave frame is independently decodable, the next one heals.
                        return true
                    } catch {
                        if !token.isStopped { onSessionEnd?() }
                        return false // session closed
                    }
                }
            }
        }
    }

    /// A frame's present reached glass, or its drawable was dropped (`presentedNs` nil: "now" on
    /// the Metal callback). End-to-end is capture→on-glass, skew-corrected by the connect-time
    /// clock offset; the display stage is decoded→on-glass, both CLOCK_REALTIME, with no offset.
    nonisolated static func stampPresented(
        _ frame: ReadyFrame, presentedNs: Int64?, issuedNs: Int64,
        endToEnd: LatencyMeter?, clockOffset: () -> Int64, hud: HudSink, stats: PresentDebugStats?
    ) {
        let atNs = presentedNs
            ?? realtimeNs(forDisplayLinkTimestamp: CACurrentMediaTime())
        endToEnd?.record(ptsNs: frame.ptsNs, atNs: atNs, offsetNs: clockOffset())
        hud.displayed(ptsNs: frame.ptsNs, decodedNs: frame.decodedNs, atNs: atNs)
        stats?.presented(
            atNs: presentedNs, issuedNs: issuedNs, ptsNs: frame.ptsNs,
            decodedNs: frame.decodedNs,
            isRepeat: frame.flags & PunktfunkConnection.userFlagRepeat != 0)
    }

    /// Latency intent: wait for the pick `at`, then everything decoded since goes to the carry.
    /// Returns the frame to show, whether the thread waited, and whether the carry kept a newer
    /// frame back; that frame goes back to the ring.
    nonisolated static func holdForPick(
        _ frame: ReadyFrame, at: CFTimeInterval, switched: Bool, period: CFTimeInterval,
        selector: inout DeadlineSelector, ring: FrameStore<ReadyFrame>, pacer: DeadlinePacer
    ) -> (show: ReadyFrame, held: Bool, carried: Bool) {
        let waited = at > CACurrentMediaTime()
        if waited { wait(untilMediaTime: at) }
        selector.noteRate(
            at: CACurrentMediaTime(), period: period, submitted: { ring.submittedTotal() })
        let frames = [frame] + ring.takeAll()
        let choice = selector.choose(frames, switched: switched)
        if let keep = choice.keep { ring.putBack(keep) }
        if waited {
            pacer.noteHeld(wakeLate: CACurrentMediaTime() - at, superseded: frames.count > 1)
        }
        return (choice.show, waited, choice.keep != nil)
    }

    /// A deadline window line: stdout under PUNKTFUNK_PRESENT_DEBUG, else the os_log mirror.
    nonisolated static func emitDeadlineLine(_ line: String) {
        if presentDebug {
            print(line)
            fflush(stdout)
        } else {
            presentLog.info("\(line, privacy: .public)")
        }
    }

    /// Block until `t` (`CACurrentMediaTime` seconds) as an absolute deadline on the mach clock
    /// that time counts in, so a late start does not stretch the wait.
    nonisolated static func wait(untilMediaTime t: CFTimeInterval) {
        mach_wait_until(UInt64(t * 1e9 * Double(timebase.denom) / Double(timebase.numer)))
    }

    private nonisolated static let timebase: mach_timebase_info_data_t = {
        var info = mach_timebase_info_data_t()
        mach_timebase_info(&info)
        return info
    }()

    /// Convert a `CADisplayLink.targetTimestamp` (CACurrentMediaTime basis) to a `CLOCK_REALTIME`
    /// nanosecond instant — the present clock the AU pts + skew offset live in. Projects to the target
    /// present time (when the frame is actually on glass), not the moment we drew.
    public nonisolated static func realtimeNs(forDisplayLinkTimestamp t: CFTimeInterval) -> Int64 {
        let caNow = CACurrentMediaTime()
        let realtimeNow = realtimeNowNs()
        return realtimeNow + Int64((t - caNow) * 1_000_000_000)
    }

    /// The exact inverse: a client `CLOCK_REALTIME` nanosecond instant (`ReadyFrame.decodedNs`)
    /// expressed on the `CACurrentMediaTime` timeline the present path schedules against.
    ///
    /// It reads the two clocks in the SAME ORDER as `realtimeNs(forDisplayLinkTimestamp:)` above
    /// and forms the same difference, so the sub-microsecond skew between the two reads is the
    /// same sign in both and cancels on a round trip.
    ///
    /// The cadence loop needs this because its rule is one domain in, SAME domain out: it is fed
    /// the decode-output instant in media time and its due time comes back in media time, with no
    /// second conversion anywhere downstream. (A constant realtime↔media offset would be absorbed
    /// by the loop's own offset estimator and need no conversion at all — but the two clocks
    /// diverge across device sleep, which is exactly why the conversion is done per frame here
    /// rather than once per session.)
    nonisolated static func mediaTimeNs(forRealtimeNs t: Int64) -> Int64 {
        let caNow = CACurrentMediaTime()
        let realtimeNow = realtimeNowNs()
        return Int64(caNow * 1_000_000_000) + (t - realtimeNow)
    }

    /// Stamp a decoded frame with when it is DUE on the source's cadence, at the moment it enters
    /// the ready store. Returns the frame untouched when the session has no clock (the latency
    /// intent).
    ///
    /// A frame whose wire pts did not survive (`ptsNs == 0` — the decoder's "unknown" value) is
    /// not on the source's timeline at all, so it is folded through `noteOffCadence`: due as soon
    /// as it is ready, and the estimate left alone. Folding "now" in would drag the offset toward
    /// this instant precisely when the loop has the least evidence.
    private nonisolated static func dated(
        _ frame: ReadyFrame, by clock: CadenceClock?, hint: FrameRateHint
    ) -> ReadyFrame {
        guard let clock else { return frame }
        let readyNs = mediaTimeNs(forRealtimeNs: frame.decodedNs)
        let interval = hint.sourceIntervalNs()
        let dueNs =
            frame.ptsNs > 0
            ? clock.dueNs(
                srcPtsNs: frame.ptsNs, readyNs: readyNs, frameIntervalNs: interval)
            : clock.noteOffCadence(readyNs: readyNs, frameIntervalNs: interval)
        var dated = frame
        dated.dueMediaTime = Double(dueNs) / 1_000_000_000
        return dated
    }
}
#endif
