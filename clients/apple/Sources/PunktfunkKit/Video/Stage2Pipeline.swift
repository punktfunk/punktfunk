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
/// Sendable; lock-guarded — decoder callbacks and the render thread cross here.
public enum FrameStorePolicy: Sendable, Equatable {
    case newestWins
    case fifo(capacity: Int)
}

public final class FrameStore<Frame>: @unchecked Sendable {
    private let lock = NSLock()
    private let capacity: Int // 1 = newest-wins semantics
    private let isFifo: Bool
    private var frames: [Frame] = []
    private var prerolled = false
    /// Submissions since the last `drainSubmitted` — the decode rate for the pf-present line.
    private var submitted = 0
    /// Smoothness accounting for the pf-present line: frames dropped by a full buffer, and
    /// runs-dry that re-armed preroll.
    private var overflowDrops = 0
    private var underflows = 0

    public init(policy: FrameStorePolicy) {
        switch policy {
        case .newestWins:
            capacity = 1
            isFifo = false
        case .fifo(let k):
            capacity = max(1, k)
            isFifo = true
        }
    }

    func submit(_ f: Frame) {
        lock.lock()
        if isFifo {
            frames.append(f)
            if frames.count > capacity {
                frames.removeFirst() // oldest goes — bounded latency, the newest keeps flowing
                overflowDrops += 1
            }
        } else {
            frames = [f] // newest wins; the replaced frame is the intended drop point
        }
        submitted += 1
        lock.unlock()
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
        if isFifo {
            frames.insert(f, at: 0)
        } else if frames.isEmpty {
            frames = [f]
        }
        lock.unlock()
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
}

/// Selects when a decoded frame enters the layer; decode and the newest-wins store are shared.
///
/// - `arrival` (stage-2): render from frame arrival. This is the macOS default and remains an
///   explicit A/B elsewhere; macOS smoothness can additionally schedule it on the vsync grid.
/// - `glass` (stage-3): admit a bounded number of presents and reopen each slot from its on-glass
///   callback. Frames decoded while closed coalesce in the store instead of joining the layer FIFO.
/// - `deadline` (stage-4): pair the newest frame with a CAMetalDisplayLink-vended drawable. This is
///   the iOS default; tvOS retains it as an A/B path because its minimum render window spans two
///   fixed-rate refreshes.
/// - `decoded`: send VideoToolbox's IOSurface-backed output directly to the system video renderer.
///   This is tvOS's latency default and avoids compressed decode buffering plus the Metal FIFO.
///
/// macOS PyroWave defaults to `glass` to prevent burst presents in its composited layer.
public enum PresentPacing: Sendable, Equatable {
    case arrival
    case glass
    case deadline
    case decoded
}

/// Direct decoded-frame handoff to AVSampleBufferDisplayLayer's background-safe renderer.
///
/// VideoToolbox already produced an IOSurface-backed YUV image, so wrapping it as an immediate
/// uncompressed sample adds no copy or second decode. Backpressure drops the frame instead of
/// building a queue; the next decoder callback supplies a fresher image. Display-link polling maps
/// the renderer's current IOSurface ID back to its capture/decode stamp for on-glass metrics.
/// The renderer owns each sample after enqueue. Sendable because AVSampleBufferVideoRenderer
/// explicitly permits background-thread enqueueing.
final class DecodedVideoSink: @unchecked Sendable {
    private struct Stamp {
        let ptsNs: UInt64
        let decodedNs: Int64
    }

    private let renderer: AVSampleBufferVideoRenderer
    private let lock = NSLock()
    private var stamps: [IOSurfaceID: Stamp] = [:]

    init(layer: AVSampleBufferDisplayLayer) {
        renderer = layer.sampleBufferRenderer
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
        stamps[surfaceID] = Stamp(ptsNs: frame.ptsNs, decodedNs: frame.decodedNs)
        lock.unlock()
        renderer.enqueue(sample)
        return true
    }

    func takeDisplayedStamp() -> (ptsNs: UInt64, decodedNs: Int64)? {
        guard #available(macOS 14.4, iOS 17.4, tvOS 17.4, *),
              let pixelBuffer = renderer.displayedPixelBuffer(),
              let surfaceID = Self.surfaceID(pixelBuffer)
        else { return nil }
        lock.lock()
        defer { lock.unlock() }
        guard let stamp = stamps.removeValue(forKey: surfaceID) else { return nil }
        return (stamp.ptsNs, stamp.decodedNs)
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
/// the other thread is never clobbered by a stale return. Internal (not private) for unit tests.
/// Sendable; lock-guarded.
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
}

/// Stage-4's stale-link ladder. A link that stops vending is relinked; a link that stalls
/// AGAIN within `rebuildWindow` of that relink means the CAMetalLayer itself is wedged, and
/// only a fresh layer cures that — so the caller rebuilds the whole presenter instead.
/// Field 2026-09-01 (iPad Pro / iOS 27): the relink vended exactly one drawable, then went
/// silent for good; reconnecting (a new layer) fixed it every time. 2 s covers that shape
/// (the second stall is re-detected ≈0.3 s after the relink) with margin; a link that ran
/// clean for longer has earned a fresh relink. Render-thread confined. Internal for tests.
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

/// The CAMetalDisplayLink delegate for deadline pacing: each per-refresh update stashes its
/// vended drawable (newest wins) and nudges the render thread — which also wakes on decoder
/// arrivals, so whichever half completes the (frame, drawable) pair triggers the present. Also
/// applies the staged frame-rate hint from the link's own thread. Retained by the link thread's
/// closure (the link holds it weak); captures only the shared boxes, never the pipeline — the
/// same no-self-capture rule as the pump/render threads.
private final class DeadlineLinkDelegate: NSObject, CAMetalDisplayLinkDelegate {
    private let stash: LatestBox<CAMetalDrawable>
    private let renderSignal: DispatchSemaphore
    private let hint: FrameRateHint
    private let stats: PresentDebugStats?
    /// Phase-locked capture's grid feed — this link IS the latch grid presents pace against.
    private let phase: PhaseReporter?
    /// Every update's vend→glass lead goes to the overlay as an OS-floor sample; its p50 is the
    /// floor iOS and tvOS take off the shown display and end-to-end. ~1 refresh is the goal; ~2
    /// means the compositor runs a frame ahead of us. Tracks VRR rate changes.
    private let hud: HudSink?
    /// The pool depth this session vends from (`startDeadlinePresenter` sets it on the layer).
    /// Carried only so the one-shot line below reports the two halves of the depth question
    /// together — a `preferredFrameLatency` of 1 against a 3-slot pool is the configuration that
    /// measured a two-refresh floor in the field, and reading either number alone hides that.
    private let drawableCount: Int
    /// The `preferredFrameLatency` this session asks for — 1 by default, PUNKTFUNK_FRAME_LATENCY
    /// for the on-device ladder (see `startDeadlinePresenter` for the ladder's design).
    private let latencyAsk: Float
    /// One-shot: log the link's preferredFrameLatency READBACK after the first re-assert. A
    /// readback differing from the ask ⇒ the system clamps the property (the one clamp signal
    /// it can give); a readback EQUAL to the ask proves nothing — only vendLeadMs does (see
    /// PresentLinkInfo's doc for the field lesson).
    private var loggedEffective = false

    init(
        stash: LatestBox<CAMetalDrawable>, renderSignal: DispatchSemaphore,
        hint: FrameRateHint, stats: PresentDebugStats?, hud: HudSink?,
        phase: PhaseReporter?, drawableCount: Int, latencyAsk: Float
    ) {
        self.stash = stash
        self.renderSignal = renderSignal
        self.hint = hint
        self.stats = stats
        self.hud = hud
        self.phase = phase
        self.drawableCount = drawableCount
        self.latencyAsk = latencyAsk
    }

    func metalDisplayLink(_ link: CAMetalDisplayLink, needsUpdate update: CAMetalDisplayLink.Update) {
        if let range = hint.drain(), link.preferredFrameRateRange != range {
            link.preferredFrameRateRange = range
        }
        // Re-assert the latency ask every update (cheap compare): it was set once before
        // add(to:), and whether a pre-add set survives scheduling is exactly the kind of
        // thing the vendLeadMs stat exists to catch — belt and braces.
        if link.preferredFrameLatency != latencyAsk { link.preferredFrameLatency = latencyAsk }
        // Publish every update, not just the first: the range is re-applied from the staged hint
        // above (mode switch / rate change), and `preferredFrameLatency` is re-asserted right
        // here — so the readback can change mid-session, and a write-once snapshot would keep
        // showing the answer to a question we have since asked again. Cheap: five stores under
        // an uncontended lock, once per refresh.
        let range = link.preferredFrameRateRange
        PresentLinkInfo.shared.publish(
            ask: latencyAsk, latency: link.preferredFrameLatency, rangeMin: range.minimum,
            rangeMax: range.maximum, drawables: drawableCount)
        if !loggedEffective {
            loggedEffective = true
            let msg = String(
                format: "deadline link up: preferredFrameLatency ask=%.2f readback=%.2f "
                    + "maxDrawables=%d range=%.0f-%.0f preferred=%.0f",
                latencyAsk, link.preferredFrameLatency, drawableCount,
                range.minimum, range.maximum, range.preferred ?? 0)
            presentLog.info("\(msg, privacy: .public)")
        }
        // The link's own pipeline depth, measured: how far ahead of glass this vend runs.
        let leadS = update.targetPresentationTimestamp - CACurrentMediaTime()
        stats?.vendLead(ms: leadS * 1000)
        // The same lead, as an OS-floor sample for the overlay.
        if leadS > 0 { hud?.floor(ns: Int64(leadS * 1_000_000_000)) }
        // Phase-locked capture: this update's target present, converted into the arrival
        // stamps' CLOCK_REALTIME domain. Per-update cost is one clock read; the reporter
        // itself flushes ~1 Hz.
        if let phase {
            var ts = timespec()
            clock_gettime(CLOCK_REALTIME, &ts)
            let nowNs = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
            phase.noteGrid(nextLatchRealNs: nowNs + Int64(leadS * 1_000_000_000))
        }
        stash.put(update.drawable)
        renderSignal.signal()
    }
}


/// Bridges the VideoToolbox decode-completion callback to the core Automatic-bitrate controller's
/// decode signal. Created as a pipeline property so the decoder's `onDecoded` callback (built in
/// `init`, before the connection exists) can capture it, then `start` binds the live connection +
/// the arming flag once known — the same "reference captured in init, configured in start" shape as
/// `recovery`/`gate`. `record` runs on VideoToolbox's callback thread; `bind` runs once on the main
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

public final class Stage2Pipeline {
    private let ring: FrameStore<ReadyFrame>
    private let presenter: MetalVideoPresenter
    private let decoder: VideoDecoder
    private let decodedSink: DecodedVideoSink?
    /// Presentation mechanism, fixed for the pipeline lifetime and resolved per session by
    /// SessionPresenter. See PresentPacing for each path's ownership and cadence.
    private let pacing: PresentPacing
    /// The glass gate's in-flight present budget (`PresentGate` capacity) — meaningful only under
    /// `.glass`; SessionPresenter resolves it per platform (see `SessionPresenter.gateDepth`).
    private let gateDepth: Int
    /// macOS smoothness: schedule at most one present on each display-link target so the FIFO
    /// store drains at display cadence. Deadline pacing has its own link and ignores this policy.
    private let vsyncPaced: Bool
    /// Source-timestamp playout for the SMOOTHNESS intent: every decoded frame is stamped with
    /// when it is due on the host's own cadence, and the present decision aims there instead of at
    /// the moment the frame happened to decode. `nil` under `latency`, whose path keeps no cadence
    /// arithmetic in it at all — see the intent gate in `init`.
    private let cadence: CadenceClock?
    private let endToEndMeter: LatencyMeter?
    /// The stats overlay's decode, display and OS-floor stamps; `start` binds the connection.
    private let hud = HudSink()
    private let recovery = KeyframeRecovery()
    /// Feeds the core Automatic-bitrate controller's decode signal from the decode callback; `start`
    /// binds the live connection + arming flag (see DecodeReport).
    private let decodeReport = DecodeReport()
    private let frameHDR = FrameHDRReporter()
    private let phaseReporter = PhaseReporter()
    /// Post-loss freeze-until-reanchor gate (shared core policy via the C ABI). Created here seeded 0;
    /// `start` reseeds it to the live connection's drop count. Captured by the decoder callbacks
    /// (which withhold concealed frames) and driven by the pump (arm on a gap, poll per iteration).
    private let gate = ReanchorGate(framesDropped: 0)
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

    /// The Metal layer the hosting view installs + sizes.
    public var layer: CAMetalLayer { presenter.layer }
    /// Deadline pacing found the layer wedged (see `LinkStallPolicy`): fires ONCE, from the
    /// render thread. The owner rebuilds the presenter on a fresh layer. Set before `start`.
    public var onPresentWedged: (@Sendable () -> Void)?

    /// `endToEndMeter` records capture→on-glass per presented frame for the A/V sync loop; the
    /// overlay's stamps reach the core through the connection. Metering never gates the
    /// presenter choice. Returns nil if Metal can't be set up (headless / no GPU) — caller
    /// falls back to the stage-1 presenter. `pacing` also selects the decoded video sink when its
    /// `displayLayer` is supplied. `gateDepth` bounds glass presents; `vsyncPaced` schedules macOS
    /// smoothness onto the ordinary display-link grid.
    public init?(
        endToEndMeter: LatencyMeter?,
        displayLayer: AVSampleBufferDisplayLayer? = nil,
        pacing: PresentPacing = .arrival,
        gateDepth: Int = 1,
        storePolicy: FrameStorePolicy = .newestWins,
        vsyncPaced: Bool = false
    ) {
        let decodedSink: DecodedVideoSink?
        if pacing == .decoded {
            guard let displayLayer else { return nil }
            decodedSink = DecodedVideoSink(layer: displayLayer)
        } else {
            decodedSink = nil
        }
        guard let presenter = MetalVideoPresenter.make() else { return nil }
        self.presenter = presenter
        self.pacing = pacing
        self.gateDepth = gateDepth
        self.vsyncPaced = vsyncPaced
        self.ring = FrameStore(policy: storePolicy)
        self.endToEndMeter = endToEndMeter
        self.decodedSink = decodedSink
        // The intent gate: source-timestamp playout is what `smooth` MEANS now, and `latency` is
        // defined as arrival-driven with no cushion — so the store policy, which is the intent's
        // only other expression (`PresentPriority.storePolicy`: smooth → fifo, latency →
        // newest-wins), is what decides whether a clock exists at all. A latency session runs the
        // same present path it ran before this existed.
        switch storePolicy {
        case .newestWins: self.cadence = nil
        case .fifo: self.cadence = CadenceClock(tuning: .snapping())
        }
        let ring = ring
        let recovery = recovery
        let renderSignal = renderSignal
        let gate = gate
        let decodeReport = decodeReport
        let frameHDR = frameHDR
        let hud = hud
        let phaseReporter = phaseReporter
        let cadence = cadence
        let rateHint = frameRateHint
        self.decoder = VideoDecoder(
            onDecoded: { frame in
                // Decode stage = received→decoded, both client CLOCK_REALTIME (offset 0 — no
                // skew applies). Stamped at decode completion, so it covers every decoded frame,
                // including ones the re-anchor gate withholds or the newest-wins ring drops.
                hud.decoded(
                    ptsNs: frame.ptsNs, receivedNs: frame.receivedNs, decodedNs: frame.decodedNs)
                // Same interval, reported to the core bitrate controller so Automatic caps at this
                // device's real decode limit instead of the network link ceiling. Every decoded
                // frame (not just presented ones), so a newest-wins drop can't hide the backlog.
                decodeReport.record(receivedNs: frame.receivedNs, decodedNs: frame.decodedNs)
                // The decoded video plane reanchors independently of host submit phase. Feeding
                // it to the phase controller adds a standing grid period without moving display.
                if pacing != .decoded { phaseReporter.noteArrival(receivedNs: frame.receivedNs) }
                // Freeze-until-reanchor: WITHHOLD a decoder-concealed post-loss frame (the gray/
                // garbage VideoToolbox returns Ok for a reference-missing delta) — don't submit it,
                // so the CAMetalLayer keeps its last good drawable on glass. The gate lifts (returns
                // present) on a proven clean re-anchor (IDR / RFI anchor / 2nd recovery mark) or the
                // bounded backstop. decoderKeyframe=false: VT doesn't flag IDRs, the wire FLAG_SOF does.
                guard gate.onDecoded(flags: frame.flags) else { return }
                if case .video(_, let isHDR) = frame.image { frameHDR.note(isHDR) }
                if let decodedSink {
                    decodedSink.submit(frame)
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
            // Async decode failure: fold it into the gate's no-output streak (which arms the freeze
            // after a short run, matching the desktop), and when that trips ask the host for a
            // fresh IDR now (infinite GOP — it wouldn't otherwise come soon). One WARN per sent
            // ask carries the OSStatus, the only trace a field log has of what VideoToolbox refused.
            onDecodeError: { status in
                if gate.onNoOutput(), recovery.request() {
                    pumpLog.warning(
                        "video: VideoToolbox refused an AU status=\(status, privacy: .public) — asked the host for a keyframe"
                    )
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
    /// A stopped pipeline is permanent; construct a new instance for another session.
    public func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil,
        onFrameHDR: (@Sendable (Bool) -> Void)? = nil
    ) {
        clockOffset = { connection.clockOffsetNs } // live (re-synced) — see the field doc
        frameHDR.bind(onFrameHDR)
        recovery.bind(connection) // arm host-keyframe recovery for this session
        decodeReport.bind(connection) // arm the Automatic-bitrate decode signal for this session
        hud.bind(connection) // the overlay's decode, display and floor stamps
        phaseReporter.bind(pacing == .decoded ? nil : connection)
        gate.reseed(framesDropped: connection.framesDropped()) // baseline the freeze to this session
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

        let token = token
        let decoder = decoder
        let recovery = recovery
        let presenter = presenter
        let pumpStopped = pumpStopped
        let reanchorGate = gate
        // PyroWave rides a different decode half: no CMFormatDescription/VideoToolbox machinery
        // (a wavelet AU has no parameter sets), no keyframe recovery or re-anchor freeze (the
        // stream is all-intra and Phase 4's partial delivery WANTS lossy frames on glass as
        // localized blur, not a freeze). The ready ring, render thread, pacing and meters are
        // shared unchanged.
        let thread: Thread
        if connection.videoCodec == .pyrowave {
            thread = Self.makePyroWavePump(
                connection: connection, token: token, pumpStopped: pumpStopped,
                ring: ring, renderSignal: renderSignal,
                device: presenter.metalDevice, queue: presenter.metalQueue,
                hud: hud, cadence: cadence, rateHint: frameRateHint,
                onFrame: onFrame, onSessionEnd: onSessionEnd, onDecodedSize: onDecodedSize,
                frameHDR: frameHDR,
                onHdrMeta: { [weak presenter] meta in presenter?.setHdrMeta(meta) })
        } else {
            thread = Thread {
            defer { pumpStopped.signal() } // let stop() join the pump (bounded) before decoder.reset()
            // Format, decoded size, straggler filter and the keyframe WANT — the same rules the
            // stage-1 pump follows, in one tested place. Loss itself goes through the gate, where
            // an RFI anchor heals it without an IDR.
            var pump = AUPumpState()
            // VideoToolbox reads the RPS itself, so a lost reference must be concealed in the
            // bitstream before submit (see HevcConcealer). Thread-confined; one per session.
            let concealer: HevcConcealer? = connection.videoCodec == .hevc ? HevcConcealer() : nil
            var wasUnrecoverable = false
            // 4:4:4 backstop: a run of decode/create failures in a 4:4:4 session means this device can't
            // decode 4:4:4 at the negotiated resolution (the HW probe clears the common case but not a
            // resolution-ceiling miss). End cleanly instead of looping on a black screen.
            var decodeFailRun = 0
            // Every iteration drains its own autorelease pool: this thread has no runloop, so
            // autoreleased VT/CM temporaries would otherwise accumulate until session end.
            // `false` = session over — exit the loop (the closure can't `break` across itself).
            var alive = true
            while alive, !token.isStopped {
                alive = autoreleasepool { () -> Bool in
                do {
                    // Background keep-alive: drain one AU (flow control + host pacing) and discard it
                    // BEFORE any VideoToolbox decode or Metal render — no GPU work off-screen. The
                    // decoder session is left intact; exitBackground requests a fresh IDR and the
                    // re-anchor gate arms on the resumed frame-index gap so concealed frames are
                    // withheld until it lands.
                    if connection.isVideoDropped {
                        _ = try connection.nextAU(timeoutMs: 100)
                        return true
                    }
                    if pump.awaitingIDR { recovery.request() }
                    // Loss recovery through the shared gate: a drop-count climb beyond the gap's
                    // credit arms the freeze and asks (the decoder conceals reference-missing
                    // deltas without an error), and an overdue freeze re-asks for the re-anchor.
                    if reanchorGate.poll(framesDropped: connection.framesDropped()) {
                        recovery.request()
                    }
                    // Drain HDR mastering metadata (0xCE) and hand it to the PRESENTER (→ CAEDRMetadata).
                    // Polled UNCONDITIONALLY (not gated on connection.isHDR, the fixed Welcome flag): the
                    // host sends 0xCE only for HDR, INCLUDING a mid-session SDR→HDR transition (a game
                    // entering HDR — the host re-inits its encoder) the Welcome flag would never reflect.
                    // Non-blocking; nil for an SDR stream.
                    if let meta = try? connection.nextHdrMeta(timeoutMs: 0) {
                        presenter.setHdrMeta(meta)
                    }
                    guard let received = try connection.nextAU(timeoutMs: 100) else { return true }
                    var au = received // the concealer may swap its bytes below
                    // A forward frame-index gap fires a throttled RFI (a clean P-frame, no IDR)
                    // and arms the freeze, credited with the gap width so the reassembler's
                    // ~120 ms-later framesDropped climb for the same loss cannot re-freeze a
                    // stream the anchor already healed. A lost anchor lapses into the gate's
                    // overdue re-ask above.
                    let gapWidth = connection.noteFrameIndexGapWidth(au.frameIndex)
                    if gapWidth > 0 { reanchorGate.arm(expectingDrops: UInt64(gapWidth)) }
                    onFrame?(au)
                    if pump.isStraggler(frameIndex: au.frameIndex) { return true }
                    var concealed = AUPumpState.Concealment.none
                    if let concealer {
                        switch concealer.conceal(au.data) {
                        case .intact:
                            concealed = .decodable
                        case .rewritten(let data):
                            concealed = .decodable
                            au = au.replacing(data: data)
                            pumpLog.notice(
                                "video: frame \(au.frameIndex, privacy: .public) names a lost reference — moved to a present picture until the re-anchor"
                            )
                        case .unrecoverable:
                            concealed = .unrecoverable
                        }
                        if concealed == .unrecoverable, !wasUnrecoverable {
                            pumpLog.warning(
                                "video: frame \(au.frameIndex, privacy: .public) names a lost reference with nothing to stand in — withholding until an IDR"
                            )
                        }
                        wasUnrecoverable = concealed == .unrecoverable
                    }
                    let step = pump.note(
                        frameIndex: au.frameIndex,
                        idrFormat: connection.videoCodec.formatDescription(fromKeyframe: au.data),
                        lossAhead: gapWidth > 0, flags: au.flags, concealed: concealed)
                    if step.straggler { return true }
                    if let size = step.newSize { onDecodedSize?(size.width, size.height) }
                    if step.startedFormatWait {
                        pumpLog.warning(
                            "video: received AUs but no decodable format (missing/unparsed parameter sets) — requesting an IDR until one seeds it"
                        )
                    }
                    if step.askKeyframe { recovery.request() }
                    // A delta between a loss and its re-anchor references the lost picture. Fed to
                    // VideoToolbox it poisons the session — every later non-IDR AU, the anchor too,
                    // comes back kVTVideoDecoderBadDataErr. Withheld, the anchor decodes and lifts.
                    if step.withhold { return true }
                    guard let f = pump.format, !token.isStopped else { return true }
                    if decoder.decode(au: au, format: f) {
                        decodeFailRun = 0
                    } else {
                        // Submit/decoder error: drop the session and re-gate on the next IDR's in-band
                        // parameter sets (a delta frame can't recover) and keep asking for that IDR.
                        decoder.reset()
                        pump.requireIDR()
                        decodeFailRun += 1
                        // ~3 s of solid failure in a 4:4:4 session (and only there — a 4:2:0 loss
                        // recovers within a GOP) ⇒ 4:4:4 isn't decodable here; end the session.
                        if connection.isChroma444, decodeFailRun >= 180 {
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
        thread.name = "punktfunk-stage2-pump"
        thread.qualityOfService = .userInteractive
        pumpJoinable = true
        thread.start()

        if decodedSink != nil { return }

        // The present half. Deadline pacing (stage-4) swaps it wholesale: a CAMetalDisplayLink
        // vends the drawables and its per-refresh updates co-drive the render thread — see
        // startDeadlinePresenter. The V-Sync policy below doesn't apply there (the link deadline-
        // times every present). Deadline sessions ALWAYS carry the stats (their pf-present line
        // streams to Console.app via presentLog — the on-device pacing decomposition).
        let debugStats =
            (presentDebug || pacing == .deadline) ? PresentDebugStats(cadence: cadence) : nil
        if pacing == .deadline {
            startDeadlinePresenter(debugStats: debugStats)
            return
        }

        // The render thread: one present per display-link signal. It owns every layer format/colour/
        // drawable interaction (see MetalVideoPresenter's threading notes); with displaySyncEnabled on,
        // nextDrawable's up-to-a-frame wait lands here instead of on main. The 100 ms timed wait is
        // only the stop-flag poll for a session whose link stopped ticking.
        let ring = ring
        let endToEndMeter = endToEndMeter
        let hud = hud
        let clockOffset = clockOffset
        let renderSignal = renderSignal
        let renderStopped = renderStopped
        // Present policy — the user's V-Sync setting (default OFF = immediate, the long-proven
        // lowest-latency behavior); PUNKTFUNK_PRESENT_MODE=immediate|vsync overrides it for A/B.
        // Resolved once per session.
        let presentMode = ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENT_MODE"]
        // `vsyncPaced` (macOS smoothness) FORCES vsync scheduling — the FIFO store must drain
        // on the display cadence, one frame per vsync, or the buffer degenerates to arrival.
        let vsyncPaced = vsyncPaced
        let vsyncEnabled = vsyncPaced || presentMode == "vsync"
            || (presentMode != "immediate"
                && connection.settings.vsync)
        let vsyncClock = vsyncClock
        // Stage-3's bounded in-flight present gate; nil = stage-2's present-on-arrival. A local
        // (like the ring) so neither the render thread nor the presented handlers capture `self`.
        let gate: PresentGate? = pacing == .glass ? PresentGate(capacity: gateDepth) : nil
        // Cadence targeting turns the store into a holding buffer: a frame comes out once it is
        // DUE, not once a present opportunity exists (§4.3). The latency intent has no clock and
        // keeps the unconditional take, byte for byte.
        let takeReady: () -> ReadyFrame? = cadence == nil
            ? { ring.take() }
            : { ring.take(dueBy: CACurrentMediaTime(), due: { $0.dueMediaTime }) }
        let renderThread = Thread {
            defer { renderStopped.signal() }
            // macOS smoothness: the vsync this thread last presented onto — at most ONE present
            // per vsync so the FIFO drains on the display's cadence. Thread-confined.
            var lastPresentTarget: CFTimeInterval = 0
            // Every iteration drains its own autorelease pool (`return` = the old `continue`):
            // this thread has no runloop, and `nextDrawable()` AUTORELEASES each CAMetalDrawable —
            // without a per-iteration pool every presented frame's drawable object (plus its
            // texture-descriptor/array retinue, ~2 MB/min at 120 fps) piles up until session end.
            while !token.isStopped { autoreleasepool {
                if renderSignal.wait(timeout: .now() + .milliseconds(100)) == .timedOut {
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                // Smoothness pacing: this vsync's present slot already taken — the frame stays
                // in the store, and the next display-link tick re-signals. (Tolerance well under
                // any refresh period; a stale clock ⇒ nil target ⇒ no dedup, present flows.)
                if vsyncPaced, let t = vsyncClock.nextVsync(after: CACurrentMediaTime()),
                   abs(t - lastPresentTarget) < 0.002 {
                    debugStats?.gatedWake()
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                // Stage-3: while a present is in flight, don't take from the ring at all — frames
                // keep coalescing there (newest wins, the intended drop point) and the presented
                // handler re-signals the moment the slot frees. Checked BEFORE the take so a gated
                // frame is never bounced through putBack.
                if let gate, !gate.tryAcquire(now: CACurrentMediaTime()) {
                    debugStats?.gatedWake()
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                guard !token.isStopped, let frame = takeReady() else {
                    gate?.release() // armed but nothing to render — don't hold the gate stale
                    debugStats?.emptyWake()
                    debugStats?.flushIfDue(ring: ring, gate: gate)
                    return
                }
                // V-Sync ON: flip on the next predicted vsync (< one period out, stale link ⇒
                // immediate — see VsyncClock). OFF: flip as soon as the GPU finishes.
                //
                // Under cadence targeting the grid is entered at the frame's DUE time rather than
                // at this instant, so two frames the host emitted one period apart land one period
                // apart on glass however unevenly they arrived. Never before `now`: a due time in
                // the past means the frame is late, not that the grid moves back.
                let now = CACurrentMediaTime()
                let presentAt = vsyncEnabled
                    ? vsyncClock.nextVsync(after: max(now, frame.dueMediaTime ?? now)) : nil
                let renderStarted = CACurrentMediaTime()
                let issuedNs = Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: renderStarted)
                let onGlass: (Int64?) -> Void = { presentedNs in
                    // Stage-3: the flip reached glass (or was dropped) — free the present slot,
                    // then re-signal so the freshest waiting ring frame goes out immediately.
                    if let gate {
                        gate.release()
                        renderSignal.signal()
                    }
                    // Fallback stamp for a dropped drawable (no system presentedTime): "now" on
                    // the Metal callback, converted to the CLOCK_REALTIME the meters live in.
                    let atNs = presentedNs
                        ?? Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: CACurrentMediaTime())
                    // End-to-end = capture→on-glass, measured directly (skew-corrected via the
                    // connect-time clock offset) — the HUD headline.
                    endToEndMeter?.record(ptsNs: frame.ptsNs, atNs: atNs, offsetNs: clockOffset())
                    // Display stage = decoded → on-glass. Both instants are client CLOCK_REALTIME,
                    // so no skew offset applies.
                    hud.displayed(ptsNs: frame.ptsNs, decodedNs: frame.decodedNs, atNs: atNs)
                    debugStats?.presented(atNs: presentedNs, issuedNs: issuedNs)
                }
                // One present tail, two decode sources: the VideoToolbox biplanar buffer or the
                // PyroWave Metal planes — the ring, pacing and meters are agnostic to which.
                let rendered: Bool
                switch frame.image {
                case .video(let pixelBuffer, let isHDR):
                    rendered = presenter.render(
                        pixelBuffer, isHDR: isHDR, presentAtMediaTime: presentAt,
                        onPresented: onGlass)
                case .planar(let planes):
                    rendered = presenter.renderPlanar(
                        planes, presentAtMediaTime: presentAt, onPresented: onGlass)
                }
                debugStats?.renderReturned(
                    ok: rendered, tookMs: (CACurrentMediaTime() - renderStarted) * 1000)
                if !rendered {
                    gate?.release() // no present registered — its handler will never fire
                    ring.putBack(frame)
                } else if vsyncPaced, let presentAt {
                    lastPresentTarget = presentAt // this vsync's slot is now taken
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
    /// CAMetalDisplayLink on its own runloop thread vends ONE drawable per refresh into the
    /// newest-wins stash, and the render thread pairs it with the newest decoded frame the
    /// moment either half completes the pair — the common case is a decoded frame presenting
    /// instantly into an already-vended drawable, which the system then latches at the upcoming
    /// refresh (`preferredFrameLatency` 1). No image queue can form (one vended drawable in
    /// flight, ever) and nothing serializes on the on-glass callback. An unpresented stashed
    /// drawable is simply replaced by the next update (back to the layer's pool), so the stash
    /// is never stale by more than a refresh while the link runs.
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
        let presenter = presenter
        let endToEndMeter = endToEndMeter
        let hud = hud
        let clockOffset = clockOffset
        let hint = frameRateHint
        let layer = presenter.layer
        let onWedged = onPresentWedged
        let stash = LatestBox<CAMetalDrawable>()
        // Cadence targeting under deadline pacing: the link's vend IS the grid snap, so the clock
        // only has to hold a frame back until it is due and the next update presents it — at most
        // one refresh later. Same holding-buffer rule as the arrival/glass loop (§4.3); latency
        // sessions have no clock and take unconditionally.
        let takeReady: () -> ReadyFrame? = cadence == nil
            ? { ring.take() }
            : { ring.take(dueBy: CACurrentMediaTime(), due: { $0.dueMediaTime }) }

        // ⭐ Shrink the drawable pool to 2 for THIS pacing — the measured fix for a present floor
        // stuck at two refreshes (field 2026-08-13, Apple TV 4K / tvOS 27: `os present +32.5` at
        // 60 Hz = 1.95 × 16.67, i.e. the system running a whole frame ahead of us).
        //
        // `maximumDrawableCount` is 3 from MetalVideoPresenter.make(), and its rationale there —
        // "more in-flight drawables before nextDrawable() has to block" — is a STAGE-2 concern.
        // Stage-4 never calls nextDrawable(): every drawable is vended by the link
        // (`update.drawable` → stash → `render(into:)`), so the third slot buys this path nothing
        // and costs it a refresh — a pool of 3 is exactly the room the compositor needs to keep
        // two presents queued ahead of scanout, which is what `preferredFrameLatency = 1` is
        // asking it not to do. Two slots is the shallowest pool that still double-buffers: one
        // vended (stashed or being rendered), one being scanned out.
        //
        // Set HERE, not on the link thread: this runs before either the render thread or the link
        // thread exists, so the layer still has a single writer (the render thread owns
        // drawableSize/format afterwards — see MetalVideoPresenter's threading notes).
        // PUNKTFUNK_DRAWABLE_COUNT=3 restores the old depth for an on-glass A/B without a
        // rebuild; values outside 2...3 are ignored (CAMetalLayer's own accepted range).
        let drawableCount =
            ProcessInfo.processInfo.environment["PUNKTFUNK_DRAWABLE_COUNT"]
                .flatMap(Int.init)
                .flatMap { (2...3).contains($0) ? $0 : nil } ?? 2
        layer.maximumDrawableCount = drawableCount

        // The frame-latency ASK (default 1 — wake as late as fits: latch the NEXT refresh).
        // PUNKTFUNK_FRAME_LATENCY overrides it for the on-device ladder. The property is a
        // FLOAT, so sub-frame asks (0.5) are expressible; whether the scheduler honours them —
        // or reacts to the property at all — is exactly what the ladder measures. Field
        // 2026-08-13 (Apple TV 4K, tvOS 27): ask 1 → vend lead 1.95 refresh periods, and the
        // readback echoed the ask throughout (it is a plain property — see PresentLinkInfo).
        // The discriminating runs, watching `os present` (the vend lead), are:
        //   ask=2   → lead grows to ~3 ⇒ the property WORKS and the tvOS floor is ~ask+1;
        //             lead stays ~2 ⇒ the property is INERT here — stop pulling this lever.
        //   ask=0.5 → any lead below ~1.9 ⇒ a real in-regime win to then tune.
        // Clamped to 0...4: negatives/NaN are meaningless, and beyond 4 asked-for frames of
        // latency nothing is being measured.
        let latencyAsk =
            ProcessInfo.processInfo.environment["PUNKTFUNK_FRAME_LATENCY"]
                .flatMap(Float.init)
                .flatMap { $0.isFinite ? min(max($0, 0), 4) : nil } ?? 1

        let phaseReporter = phaseReporter
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
                    stash: stash, renderSignal: renderSignal, hint: hint, stats: debugStats,
                    hud: hud, phase: phaseReporter,
                    drawableCount: drawableCount, latencyAsk: latencyAsk)
                let link = CAMetalDisplayLink(metalLayer: layer)
                link.preferredFrameLatency = latencyAsk // see the ladder note above
                if let range = hint.drain() { link.preferredFrameRateRange = range }
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

        // Stale-link watchdog threshold. Stage-4 owns NO drawable source of its own — every
        // drawable arrives as `update.drawable`, so a link that stops calling back is a stream
        // that never presents again (the frozen picture keeps audio and input alive, so it reads
        // as a hang, not a disconnect). Field 2026-08-28, iPad Pro / iOS 27 over Tailscale: three
        // presents returned `presentedTime == 0` against the 2-slot pool, the link went silent
        // mid-second, and `pf-present` then logged `ok=0 noDrawable=120 vendLeadMs n=0` until the
        // user quit — twice in one session, both times cured instantly by reconnecting.
        // 0.25 s is ~30 refreshes at 120 Hz against a normal vend wait of one refresh, so it
        // cannot fire on ordinary phase jitter; the cost of a false positive is one relinked
        // frame, the cost of missing it is the whole session.
        let linkStaleAfter: CFTimeInterval = 0.25

        let renderThread = Thread {
            defer { renderStopped.signal() }
            // The live link generation's stop flag — nil until the first frame starts one.
            // Render-thread confined (only this thread starts, retires or reads it).
            var linkStop: StopFlag?
            // When the link last handed over a drawable, for the stale-link watchdog. Reset on
            // every (re)start too, so a fresh link gets its first vend before it can be judged.
            var lastVend = CACurrentMediaTime()
            // Relink once, rebuild if it stalls again (see LinkStallPolicy). `wedged` latches:
            // the rebuild replaces this pipeline, so the watchdog stops after one report.
            var stallPolicy = LinkStallPolicy()
            var wedged = false
            // Per-iteration autorelease pool — same contract as the arrival/glass loop (the
            // vended drawable and its retinue are autoreleased objects on a runloop-less thread).
            while !token.isStopped { autoreleasepool {
                if renderSignal.wait(timeout: .now() + .milliseconds(100)) == .timedOut {
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                }
                // Present needs the PAIR — frame first. The frame drives the layer reconcile,
                // which must run even when NO drawable is vended yet: the link vends from the
                // layer's CURRENT config, so drawableSize/format have to be right before a vend
                // can succeed at all (see reconcileLayer — the session-start bootstrap, where
                // the layer still has its initial 0×0 size and every vend fails allocation).
                guard !token.isStopped, let frame = takeReady() else {
                    debugStats?.emptyWake()
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                }
                switch frame.image {
                case .video(let pixelBuffer, let isHDR):
                    presenter.reconcileLayer(
                        decodedSize: CGSize(
                            width: CVPixelBufferGetWidth(pixelBuffer),
                            height: CVPixelBufferGetHeight(pixelBuffer)),
                        isHDR: isHDR, tenBitSDR: MetalVideoPresenter.tenBitBuffer(pixelBuffer))
                case .planar(let planes):
                    presenter.reconcileLayer(
                        decodedSize: CGSize(width: planes.width, height: planes.height),
                        isHDR: planes.pq)
                }
                // First frame: the layer now has a real config — start vending (see startLink).
                if linkStop == nil {
                    let stop = StopFlag()
                    linkStop = stop
                    lastVend = CACurrentMediaTime()
                    startLink(stop)
                }
                guard let drawable = stash.take() else {
                    // No vend yet (session start: the reconcile above just unblocked the
                    // allocator, the link's next update delivers; steady state: decode beat the
                    // link's phase). putBack keeps newest-wins — a fresher decode replaces this
                    // frame while it waits, and the update's signal retries the pairing.
                    ring.putBack(frame)
                    debugStats?.noDrawableWake()
                    // …unless the link has gone silent (see `linkStaleAfter`). Only a decoded
                    // frame reaches here, so a quiet stream never trips this. First stall:
                    // retire the generation and relink (the retired thread invalidates within
                    // one ≤100 ms runloop poll). A stall right after that relink: the layer
                    // is wedged — hand the session to the owner for a rebuild (LinkStallPolicy).
                    let stalledFor = CACurrentMediaTime() - lastVend
                    if let stale = linkStop, !wedged, stalledFor > linkStaleAfter {
                        let ms = Int(stalledFor * 1000)
                        stale.stop()
                        if stallPolicy.onStall(now: CACurrentMediaTime()) == .rebuild,
                           let onWedged {
                            wedged = true
                            presentLog.error(
                                "stage4: link stalled \(ms) ms again right after a relink — layer wedged, rebuilding the presenter")
                            onWedged()
                        } else {
                            let stop = StopFlag()
                            linkStop = stop
                            lastVend = CACurrentMediaTime()
                            startLink(stop)
                            presentLog.error(
                                "stage4: link stalled \(ms) ms with no vend — retiring it, relinking")
                        }
                    }
                    debugStats?.flushIfDue(ring: ring, gate: nil)
                    return
                }
                let renderStarted = CACurrentMediaTime()
                lastVend = renderStarted
                let issuedNs = Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: renderStarted)
                let onGlass: (Int64?) -> Void = { presentedNs in
                    let atNs = presentedNs
                        ?? Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: CACurrentMediaTime())
                    endToEndMeter?.record(ptsNs: frame.ptsNs, atNs: atNs, offsetNs: clockOffset())
                    hud.displayed(ptsNs: frame.ptsNs, decodedNs: frame.decodedNs, atNs: atNs)
                    debugStats?.presented(atNs: presentedNs, issuedNs: issuedNs)
                }
                let rendered: Bool
                switch frame.image {
                case .video(let pixelBuffer, let isHDR):
                    rendered = presenter.render(
                        pixelBuffer, isHDR: isHDR, into: drawable, onPresented: onGlass)
                case .planar(let planes):
                    rendered = presenter.renderPlanar(
                        planes, into: drawable, onPresented: onGlass)
                }
                debugStats?.renderReturned(
                    ok: rendered, tookMs: (CACurrentMediaTime() - renderStarted) * 1000)
                if !rendered {
                    // The vended drawable is spent either way (an unused/mismatched one drops
                    // back to the pool); the frame retries on the link's next vend. A format
                    // mismatch (mid-session HDR flip caught between the layer reconfigure and
                    // the next vend) self-heals the same way — see encodePresent's guard.
                    ring.putBack(frame)
                }
                debugStats?.flushIfDue(ring: ring, gate: nil)
            } }
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
        #if os(tvOS)
        if let stamp = decodedSink?.takeDisplayedStamp() {
            let atNs = Self.realtimeNs(forDisplayLinkTimestamp: displayedMediaTime)
            endToEndMeter?.record(ptsNs: stamp.ptsNs, atNs: atNs, offsetNs: clockOffset())
            hud.displayed(ptsNs: stamp.ptsNs, decodedNs: stamp.decodedNs, atNs: atNs)
        }
        #endif
        if pacing != .decoded { renderSignal.signal() }
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

    #if os(macOS)
    /// Forward the windowed present mechanism (MAIN thread — see
    /// `MetalVideoPresenter.setWindowedPresent`, the DCP swapID-panic mitigation).
    func setWindowedPresent(_ mode: WindowedPresentMode) {
        presenter.setWindowedPresent(mode)
    }

    /// The windowed `surface` present target the hosting SessionPresenter installs as a sibling
    /// ABOVE `layer` (transparent while unused — see `MetalVideoPresenter.surfaceLayer`).
    var surfaceLayer: CALayer { presenter.surfaceLayer }
    #endif

    /// Forward the display's current EDR headroom to the presenter (MAIN thread — a `UIScreen`
    /// read). tvOS flips HDR presentation between PQ passthrough and the in-shader tone-map on
    /// it; see `MetalVideoPresenter.setDisplayHeadroom`.
    public func setDisplayHeadroom(_ headroom: CGFloat) {
        presenter.setDisplayHeadroom(headroom)
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
        recovery.bind(nil) // stop requesting keyframes once the session is torn down
        phaseReporter.bind(nil) // and stop phase reports toward the dead connection
    }

    deinit {
        token.stop()
        renderSignal.signal() // wake the render thread so it can observe the stop and exit
    }

    /// The PyroWave pump: AUs go straight into the Metal wavelet decoder (no VideoToolbox, no
    /// format descriptions), decoded planes ride the same ready ring / render thread. All-intra
    /// stream, so none of the VT pump's recovery machinery applies: keyframe/RFI requests are
    /// silenced host-side for this codec, and a lossy (partial-delivery) frame is MEANT to
    /// present as localized blur — never a freeze. Static + capture-by-parameter for the same
    /// reason the VT pump avoids capturing `self` (a missed stop must not leak a live pipeline).
    private static func makePyroWavePump(
        connection: PunktfunkConnection, token: StopFlag, pumpStopped: DispatchSemaphore,
        ring: FrameStore<ReadyFrame>, renderSignal: DispatchSemaphore,
        device: MTLDevice, queue: MTLCommandQueue,
        hud: HudSink, cadence: CadenceClock?, rateHint: FrameRateHint,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)?,
        frameHDR: FrameHDRReporter,
        onHdrMeta: (@Sendable (PunktfunkConnection.HdrMeta) -> Void)?
    ) -> Thread {
        // The chunk-aligned parse window = the session's negotiated shard payload (Welcome);
        // the 64-byte floor mirrors the Rust client's guard against a nonsense value.
        let windowSize = max(64, Int(connection.shardPayload))
        return Thread {
            defer { pumpStopped.signal() }
            // Compiles the two compute kernels on the session's first frames' thread — ~tens of
            // ms, once per session. Failure = this device can't run the negotiated codec (the
            // advertisement probe should have prevented this); end the session cleanly.
            guard let decoder = MetalWaveletDecoder(device: device, queue: queue) else {
                if !token.isStopped { onSessionEnd?() }
                return
            }
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
                            _ = try connection.nextAU(timeoutMs: 100)
                            return true
                        }
                        // Mastering metadata (0xCE), same as the VideoToolbox pump: a PQ PyroWave
                        // session drives the same EDR machinery, and without this it tone-maps
                        // against the bare reference-white anchor with no mastering volume or
                        // content-light level for the whole session.
                        if let meta = try? connection.nextHdrMeta(timeoutMs: 0) {
                            onHdrMeta?(meta)
                        }
                        guard let au = try connection.nextAU(timeoutMs: 100) else { return true }
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
                        let submitted = decoder.decode(
                            au: au.data, chunkAligned: chunkAligned, windowSize: windowSize
                        ) { planes in
                            // Metal completed-handler thread — stamp + enqueue, don't block
                            // (the exact contract of the VT output callback).
                            guard let planes else { return }
                            var ts = timespec()
                            clock_gettime(CLOCK_REALTIME, &ts)
                            let decodedNs =
                                Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
                            hud.decoded(
                                ptsNs: ptsNs, receivedNs: receivedNs, decodedNs: decodedNs)
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

    /// Convert a `CADisplayLink.targetTimestamp` (CACurrentMediaTime basis) to a `CLOCK_REALTIME`
    /// nanosecond instant — the present clock the AU pts + skew offset live in. Projects to the target
    /// present time (when the frame is actually on glass), not the moment we drew.
    public static func realtimeNs(forDisplayLinkTimestamp t: CFTimeInterval) -> Int64 {
        let caNow = CACurrentMediaTime()
        var ts = timespec()
        clock_gettime(CLOCK_REALTIME, &ts)
        let realtimeNow = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
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
    static func mediaTimeNs(forRealtimeNs t: Int64) -> Int64 {
        let caNow = CACurrentMediaTime()
        var ts = timespec()
        clock_gettime(CLOCK_REALTIME, &ts)
        let realtimeNow = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
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
    private static func dated(
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
