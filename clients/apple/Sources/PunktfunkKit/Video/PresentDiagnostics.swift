// Present-path diagnostics: the once-a-second pacing line, the display link's advertised
// timing, and the phase reporter behind PUNKTFUNK_PRESENT_DEBUG. None of it participates
// in presenting a frame — it only watches — so it lives beside the pipeline, not in it.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import IOSurface
import Metal
import PunktfunkShared
import QuartzCore
import os

/// The pf-present line's os_log mirror — same category as the presenter's own.
private let presentLog = ClientLog(category: "present")

/// The deadline link's frame-latency ASK and property READBACK, published for the HUD to render.
///
/// A readback is NOT a grant. `preferredFrameLatency` is a plain read-write float
/// (CAMetalDisplayLink.h carries no doc contract), so reading it returns whatever we last
/// stored unless the system clamps the setter. The number that tells the truth about scheduling
/// is the vend lead (the HUD's `os present` floor), never this property. A readback that DIFFERS
/// from the ask is the one clamp signal the API gives, and the ask must be visible on screen
/// because on tvOS no log is reachable.
///
/// A process-global rather than a parameter threaded through SessionModel → StreamView →
/// controller → SessionPresenter → Stage2Pipeline → delegate: it is write-once-per-session
/// diagnostics. Reset by `clear()` at session start so a stale session's answer is never read
/// as this one's.
public final class PresentLinkInfo: @unchecked Sendable {
    public static let shared = PresentLinkInfo()
    private let lock = NSLock()
    private var ask: Float = 0
    private var latency: Float = 0
    private var rangeMin: Float = 0
    private var rangeMax: Float = 0
    private var drawables: Int = 0
    private var present = false

    private init() {}

    func publish(ask: Float, latency: Float, rangeMin: Float, rangeMax: Float, drawables: Int) {
        lock.lock()
        self.ask = ask
        self.latency = latency
        self.rangeMin = rangeMin
        self.rangeMax = rangeMax
        self.drawables = drawables
        present = true
        lock.unlock()
    }

    /// Session start — a link that never comes up must not leave the previous one's answer up.
    public func clear() {
        lock.lock()
        present = false
        lock.unlock()
    }

    /// `nil` until the link's first update (or on a non-deadline rung, which has no link).
    public func snapshot()
        -> (ask: Float, latency: Float, rangeMin: Float, rangeMax: Float, drawables: Int)?
    {
        lock.lock()
        defer { lock.unlock() }
        return present ? (ask, latency, rangeMin, rangeMax, drawables) : nil
    }
}

/// What the hosting screen can do, read by the view that owns the window: the refresh range and
/// the step the refresh INTERVAL moves in between the two (`NSScreen.displayUpdateGranularity`;
/// 0 = any interval). A fixed panel has `minHz == maxHz`. iOS exposes only the ceiling.
public struct PanelInfo: Equatable, Sendable {
    public var minHz: Double
    public var maxHz: Double
    public var granularity: Double

    public init(minHz: Double, maxHz: Double, granularity: Double = 0) {
        self.minHz = minHz
        self.maxHz = maxHz
        self.granularity = granularity
    }

    public var isAdaptive: Bool { maxHz - minHz > 0.5 }
    /// The shortest refresh interval, seconds; 0 when unknown.
    public var minInterval: Double { maxHz > 0 ? 1 / maxHz : 0 }

    /// A glass interval in panel steps: 1 = the fastest refresh. An adaptive panel with a
    /// granularity steps its interval up from the minimum in that unit; anything else is a
    /// multiple of the fastest refresh. 0 when the panel is unknown.
    public func gridUnits(interval: Double) -> Int {
        guard minInterval > 0 else { return 0 }
        if isAdaptive, granularity > 0 {
            return max(0, 1 + Int(((interval - minInterval) / granularity).rounded()))
        }
        return max(0, Int((interval / minInterval).rounded()))
    }

    public var description: String {
        String(format: "%.0f-%.0fHz g=%.2f", minHz, maxHz, granularity * 1000)
    }
}

/// Deadline pacing's staged frame-rate hint, and — on every pacing — the session's nominal source
/// interval (`sourceIntervalNs`) and the hosting panel. SessionPresenter pushes the stream rate from the
/// MAIN thread (session start + every layout/Reconfigure); the link's own thread drains and
/// applies it, so the CAMetalDisplayLink is only ever touched from the thread that runs it. The
/// range is pinned (`range(hz:)`), with no idle ramp-down: a link with a low floor idles toward
/// it on a static scene (infinite GOP ⇒ no frames), and the first damage frame after idle would
/// wait out a slow tick before it could present. Sendable; lock-guarded.
final class FrameRateHint: @unchecked Sendable {
    private let lock = NSLock()
    private var pending: CAFrameRateRange?
    private var streamHz: Float = 0
    private var panelInfo = PanelInfo(minHz: 0, maxHz: 0)
    private var paceName = "starting"
    /// The hosting panel, staged from main on start and every layout (a window can move screens).
    func stagePanel(_ info: PanelInfo) {
        lock.lock()
        panelInfo = info
        lock.unlock()
    }
    func panel() -> PanelInfo {
        lock.lock()
        defer { lock.unlock() }
        return panelInfo
    }
    /// The session's resolved present policy name, for the pf-present `pace=` field.
    func stagePace(_ name: String) {
        lock.lock()
        paceName = name
        lock.unlock()
    }
    func pace() -> String {
        lock.lock()
        defer { lock.unlock() }
        return paceName
    }
    func stage(hz: Float) {
        guard hz > 0 else { return }
        lock.lock()
        streamHz = hz
        pending = Self.range(hz: hz)
        lock.unlock()
    }
    func drain() -> CAFrameRateRange? {
        lock.lock()
        defer { lock.unlock() }
        let p = pending
        pending = nil
        return p
    }
    /// The stream rate's range as it stands, for a link started mid-session: `drain` hands a
    /// CHANGE to the live link, and a fresh link that only drained would run without a rate.
    func current() -> CAFrameRateRange? {
        lock.lock()
        defer { lock.unlock() }
        return streamHz > 0 ? Self.range(hz: streamHz) : nil
    }
    /// The nominal SOURCE interval in nanoseconds — the cadence clock's cushion ceiling. Read on
    /// every pacing, not just deadline: this box is where the negotiated stream rate already
    /// lives, staged from main on session start and every Reconfigure, and the decode-completion
    /// thread needs it under a lock. 0 = not known yet, which the clock handles by running its
    /// cushion uncapped (the shared core's own behaviour for a zero interval).
    func sourceIntervalNs() -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        return streamHz > 0 ? Int64(1_000_000_000.0 / Double(streamHz)) : 0
    }
    private static func range(hz: Float) -> CAFrameRateRange {
        #if os(tvOS)
        // A TV is fixed-rate, so all three bounds pin to the stream rate: a range is a promise
        // about how variable our cadence may be, and a scheduler handed 60…120 on a 60 Hz panel
        // keeps a refresh of slack in hand.
        return CAFrameRateRange(minimum: hz, maximum: hz, preferred: hz)
        #else
        // Pinned at the panel's top rate: a frame shows at the first refresh after its pick, and
        // a lower minimum lets the system drop the panel mid-stream (judder). UIKit samples
        // touch and Pencil at the panel's cadence, so the pin also keeps their full rate.
        let cap = max(hz, 120)
        return CAFrameRateRange(minimum: cap, maximum: cap, preferred: cap)
        #endif
    }
}

extension CAMetalDrawable {
    /// The key `PresentFlight` files this drawable under: its drawable ID, or on the simulator
    /// SDKs, which have none, the object's identity.
    var flightID: Int {
        #if targetEnvironment(simulator)
        return ObjectIdentifier(self).hashValue
        #else
        return drawableID
        #endif
    }
}

/// The deadline path's last vends and what became of each, logged when the link stalls: which
/// drawables were still out, and how each present before the stall sat against its deadline.
/// Lock-guarded: the link, render and Metal callback threads all write.
final class PresentFlight: @unchecked Sendable {
    /// 1.6 s of vends at 60 Hz.
    static let capacity = 96

    private struct Vend {
        let id: Int
        /// The pool slot: three surfaces recur while the drawable IDs count up.
        let surface: UInt32
        let vendAt: CFTimeInterval, deadline: CFTimeInterval, target: CFTimeInterval
        /// `-` untouched, `X` expired, `S` skipped, `I` issued, `F` render failed.
        var fate: Character = "-"
        var fateAt: CFTimeInterval = 0
        /// The decoded frame's surface an issued present samples; 0 without one.
        var source: UInt32 = 0
        var gpu: (start: CFTimeInterval, end: CFTimeInterval)?
        /// Set by the presented handler: glass past the target, or dropped.
        var seenAt: CFTimeInterval?
        var lateMs: Double?
    }

    private let lock = NSLock()
    private var vends: [Vend] = []

    func vend(_ v: VendedDrawable) {
        vend(
            id: v.drawable.flightID,
            surface: v.drawable.texture.iosurface.map { IOSurfaceGetID($0) } ?? 0,
            at: v.vendAt, deadline: v.deadline, target: v.target)
    }

    func vend(
        id: Int, surface: UInt32, at vendAt: CFTimeInterval, deadline: CFTimeInterval,
        target: CFTimeInterval
    ) {
        lock.lock()
        if vends.count >= Self.capacity { vends.removeFirst() }
        vends.append(Vend(
            id: id, surface: surface, vendAt: vendAt, deadline: deadline, target: target))
        lock.unlock()
    }

    func fate(_ id: Int, _ fate: Character, source: UInt32 = 0) {
        let now = CACurrentMediaTime()
        update(id) { $0.fate = fate; $0.fateAt = now; $0.source = source }
    }

    func gpu(_ id: Int, start: CFTimeInterval, end: CFTimeInterval) {
        update(id) { $0.gpu = (start, end) }
    }

    /// `lateMs` nil: the system dropped the present.
    func glass(_ id: Int, lateMs: Double?) {
        let now = CACurrentMediaTime()
        update(id) { $0.seenAt = now; $0.lateMs = lateMs }
    }

    private func update(_ id: Int, _ change: (inout Vend) -> Void) {
        lock.lock()
        if let i = vends.lastIndex(where: { $0.id == id }) { change(&vends[i]) }
        lock.unlock()
    }

    /// One line per vend, oldest first, and the record starts over. `vendMs` and `seenMs` count
    /// back from now, `fate` from the vend, `gpuMs` from the deadline (positive after it).
    func drain(_ why: String) -> [String] {
        lock.lock()
        let all = vends
        vends.removeAll(keepingCapacity: true)
        lock.unlock()
        let now = CACurrentMediaTime()
        func ms(_ s: CFTimeInterval) -> String { String(format: "%.2f", s * 1000) }
        return all.enumerated().map { i, v in
            let glass: String
            if let seenAt = v.seenAt {
                glass = (v.lateMs.map { String(format: "%.2f", $0) } ?? "drop")
                    + " seenMs=" + ms(seenAt - now)
            } else {
                glass = v.fate == "I" ? "out" : "-"
            }
            let gpu = v.gpu.map { ms($0.start - v.deadline) + ".." + ms($0.end - v.deadline) }
            return "pf-flight \(why) \(i + 1)/\(all.count) id=\(v.id) surf=\(v.surface)"
                + " vendMs=\(ms(v.vendAt - now)) dlMs=\(ms(v.deadline - v.vendAt))"
                + " tgtMs=\(ms(v.target - v.vendAt)) src=\(v.source)"
                + " fate=\(v.fate)@\(v.fate == "-" ? "na" : ms(v.fateAt - v.vendAt))"
                + " gpuMs=\(gpu ?? "na") glass=\(glass)"
        }
    }
}

/// The once-a-second `pf-present` line, on every pacing: decode rate, render outcomes, the
/// slowest render call (≈ nextDrawable wait), and what glass did — on-glass intervals as a
/// histogram in panel steps (`judder` = the share outside the modal step), how far each interval
/// strayed from the source's own spacing (`cadErrMs`), and decoded→glass (`displayMs`).
/// Always to os_log; stdout under PUNKTFUNK_PRESENT_DEBUG=1. Lock-guarded — `presented` lands
/// on a Metal callback thread, the tvOS video plane's on main.
final class PresentDebugStats: @unchecked Sendable {
    /// The session's cadence loop, for the line's `cadence` segment — `nil` under the latency
    /// intent. `late` counts frames whose due time had already passed when they became
    /// presentable: the direct signal that the cushion is too small.
    private let cadence: CadenceClock?
    private let lock = NSLock()
    private var last = CACurrentMediaTime()
    private var decoded = 0, decodedRepeats = 0, shownRepeats = 0
    private var ok = 0, failed = 0, empty = 0, dropped = 0, gated = 0, noDrawable = 0
    private var maxRenderMs = 0.0
    private var lastGlassNs: Int64 = 0
    /// The previous on-glass frame's source pts, for `cadErrMs`; 0 = no cadence reference (a
    /// repeat, or a frame whose pts did not survive).
    private var lastGlassPtsNs: UInt64 = 0
    private var glassDeltasMs: [Double] = []
    private var cadenceErrMs: [Double] = []
    /// (glass step, source step) in ms per consecutive shown pair, for `glitch=`.
    private var steps: [(glass: Double, source: Double)] = []
    private var displayMs: [Double] = []
    /// Present-issue → on-glass delay per frame (system presentedTime minus the render call's
    /// start) — the DIRECT decomposition of the display stage: ring/pairing wait lives upstream
    /// of it, queue + present-pipeline cost inside it. Standing queue reads as ~n×period here;
    /// a healthy latch reads under one period.
    private var latchMs: [Double] = []
    /// Deadline pacing: the link's own pipeline depth — `targetPresentationTimestamp - now` at
    /// each update. ~1 period means preferredFrameLatency=1 is honored (a vended drawable can
    /// reach glass at the NEXT refresh); ~2 periods means the system is running a frame ahead
    /// and one whole refresh of the display stage lives INSIDE the link, not in our pairing.
    private var vendLeadMs: [Double] = []
    /// Presented-but-not-yet-on-glass drawables right now / the window's peak — the direct
    /// measurement of the layer image-queue depth the stage-3 gate exists to bound (stage-2 on a
    /// 120 Hz panel saturates this at ~maximumDrawableCount; stage-3 pegs it at the gate depth).
    private var inFlight = 0
    private var maxInFlight = 0
    /// The session's pace name for the line's `pace=` field, read per line.
    private let pace: () -> String
    /// The ordinary link's last reported period in seconds, for `linkMs` — 0 before the first
    /// tick and under deadline pacing (no ordinary link).
    private let linkPeriod: () -> CFTimeInterval
    /// The hosting panel, read live per line — a window can move to another screen.
    private let panel: () -> PanelInfo
    /// The Direct gate: presents it issued this window (`direct=`), and its mode switches.
    private var directPresents = 0
    private let scanoutSwitches: () -> Int

    init(
        cadence: CadenceClock?, pace: @escaping () -> String,
        linkPeriod: @escaping () -> CFTimeInterval,
        panel: @escaping () -> PanelInfo = { PanelInfo(minHz: 0, maxHz: 0) },
        scanoutSwitches: @escaping () -> Int = { 0 }
    ) {
        self.cadence = cadence
        self.pace = pace
        self.linkPeriod = linkPeriod
        self.panel = panel
        self.scanoutSwitches = scanoutSwitches
    }

    /// One present issued through the Direct gate.
    func directPresent() { lock.lock(); directPresents += 1; lock.unlock() }

    /// Decoder output, every frame on every pacing — before the re-anchor gate and the store.
    func decoded(isRepeat: Bool) {
        lock.lock()
        decoded += 1
        if isRepeat { decodedRepeats += 1 }
        lock.unlock()
    }

    func emptyWake() { lock.lock(); empty += 1; lock.unlock() }

    /// A wake that found the stage-3 gate closed (a present still in flight) — the frame stays in
    /// the ring for the handler's re-signal. Includes display-link ticks while gated; a high count
    /// is normal, it just shows the gate working.
    func gatedWake() { lock.lock(); gated += 1; lock.unlock() }

    /// Deadline pacing: a decoded frame is waiting but the link hasn't vended this interval's
    /// drawable yet — the frame presents on the link's next update. A high count just means
    /// decode outruns the link's phase; the wait is bounded by one refresh.
    func noDrawableWake() { lock.lock(); noDrawable += 1; lock.unlock() }

    /// Deadline pacing, LINK thread: one update's vend-to-target distance (see `vendLeadMs`).
    func vendLead(ms: Double) { lock.lock(); vendLeadMs.append(ms); lock.unlock() }

    func renderReturned(ok rendered: Bool, tookMs: Double) {
        lock.lock()
        if rendered {
            ok += 1
            inFlight += 1
            maxInFlight = max(maxInFlight, inFlight)
        } else {
            failed += 1
        }
        maxRenderMs = max(maxRenderMs, tookMs)
        lock.unlock()
    }

    /// One frame reached glass (`atNs`, system-stamped; nil = dropped). `ptsNs`/`decodedNs` are
    /// the frame's own stamps; a repeat carries no source cadence, so it ends a `cadErr` pair.
    func presented(
        atNs: Int64?, issuedNs: Int64, ptsNs: UInt64 = 0, decodedNs: Int64 = 0,
        isRepeat: Bool = false
    ) {
        lock.lock()
        inFlight = max(0, inFlight - 1) // clamp: the handler can beat renderReturned's increment
        if isRepeat { shownRepeats += 1 }
        if let atNs {
            if lastGlassNs > 0 {
                let glassDeltaNs = atNs - lastGlassNs
                glassDeltasMs.append(Double(glassDeltaNs) / 1e6)
                if lastGlassPtsNs > 0, ptsNs > lastGlassPtsNs, !isRepeat {
                    let srcDeltaNs = Int64(ptsNs - lastGlassPtsNs)
                    cadenceErrMs.append(Double(abs(glassDeltaNs - srcDeltaNs)) / 1e6)
                    steps.append((Double(glassDeltaNs) / 1e6, Double(srcDeltaNs) / 1e6))
                }
            }
            lastGlassNs = atNs
            lastGlassPtsNs = isRepeat ? 0 : ptsNs
            latchMs.append(Double(atNs - issuedNs) / 1e6)
            if decodedNs > 0 { displayMs.append(Double(atNs - decodedNs) / 1e6) }
        } else {
            dropped += 1
        }
        lock.unlock()
    }

    /// Glass intervals bucketed in panel steps, largest bucket first (`3:210,4:130`), and the
    /// share outside the modal step. A 35 fps source on a 120 Hz grid legitimately alternates
    /// 3/4; `cadErrMs` says whether that alternation follows the source.
    static func gridHistogram(deltasMs: [Double], panel: PanelInfo) -> (hist: String, judder: Double) {
        guard !deltasMs.isEmpty, panel.minInterval > 0 else { return ("", 0) }
        var counts: [Int: Int] = [:]
        for d in deltasMs { counts[panel.gridUnits(interval: d / 1000), default: 0] += 1 }
        let sorted = counts.sorted { $0.value != $1.value ? $0.value > $1.value : $0.key < $1.key }
        let hist = sorted.prefix(4).map { "\($0.key):\($0.value)" }.joined(separator: ",")
        return (hist, 1 - Double(sorted[0].value) / Double(deltasMs.count))
    }

    /// Shown pairs whose glass step, in panel refreshes, is longer than their source step (a
    /// repeat) or shorter (a skip). Exact at 1:1 and 2:1; a fractional cadence counts its own
    /// alternation.
    static func glitches(
        steps: [(glass: Double, source: Double)], refreshMs: Double
    ) -> (repeats: Int, skips: Int) {
        guard refreshMs > 0 else { return (0, 0) }
        var rep = 0, skip = 0
        for s in steps {
            let g = (s.glass / refreshMs).rounded(), c = (s.source / refreshMs).rounded()
            if g > c { rep += 1 } else if g < c { skip += 1 }
        }
        return (rep, skip)
    }

    /// The window's per-frame samples so far, for tests.
    func glassSamples() -> (cadenceErrMs: [Double], displayMs: [Double], repeats: (Int, Int)) {
        lock.lock()
        defer { lock.unlock() }
        return (cadenceErrMs, displayMs, (decodedRepeats, shownRepeats))
    }

    private static func percentiles(_ values: [Double]) -> (p50: Double, p95: Double, max: Double) {
        guard !values.isEmpty else { return (0, 0, 0) }
        let sorted = values.sorted()
        return (sorted[sorted.count / 2], sorted[min(sorted.count - 1, sorted.count * 95 / 100)],
                sorted[sorted.count - 1])
    }

    func flushIfDue(ring: FrameStore<ReadyFrame>, gate: PresentGate?) {
        lock.lock()
        let now = CACurrentMediaTime()
        guard now - last >= 1 else { lock.unlock(); return }
        last = now
        let smoothing = ring.drainSmoothing()
        let deltas = glassDeltasMs.sorted()
        let p50 = deltas.isEmpty ? 0 : deltas[deltas.count / 2]
        let dMax = deltas.last ?? 0
        let latches = latchMs.sorted()
        let latchP50 = latches.isEmpty ? 0 : latches[latches.count / 2]
        let latchMax = latches.last ?? 0
        let vends = vendLeadMs.sorted()
        let vendP50 = vends.isEmpty ? 0 : vends[vends.count / 2]
        let vendMax = vends.last ?? 0
        let inflightMax = maxInFlight
        let panel = panel()
        let (hist, judder) = Self.gridHistogram(deltasMs: glassDeltasMs, panel: panel)
        let cadErr = Self.percentiles(cadenceErrMs)
        let display = Self.percentiles(displayMs)
        let glitch = Self.glitches(steps: steps, refreshMs: panel.minInterval * 1000)
        let glassLine = String(
            format: " displayMs p50=%.1f p95=%.1f cadErrMs p50=%.2f p95=%.2f n=%d "
                + "grid=%.2f hist=%@ judder=%.2f glitch=%d/%d repeats=%d/%d panel=%@",
            display.p50, display.p95, cadErr.p50, cadErr.p95, cadenceErrMs.count,
            panel.minInterval * 1000, hist, judder, glitch.repeats, glitch.skips,
            decodedRepeats, shownRepeats, panel.description)
        // Loop health, appended only where a loop exists — `late`/`frames` is the cushion
        // criterion and `reanchor` says whether the estimate is tracking at all.
        let loop = cadence?.health()
        let cadenceLine =
            loop.map {
                String(
                    format: " cadence late=%llu/%llu reanchor=%llu jitterUs=%lld cushionUs=%lld "
                        + "skewNs=%lld",
                    $0.late, $0.frames, $0.reanchors, $0.jitterNs / 1000, $0.cushionNs / 1000,
                    $0.skewNs)
            } ?? ""
        let line = String(
            format: "pf-present pace=%@ linkMs=%.2f decoded=%d ok=%d fail=%d empty=%d "
                + "gated=%d noDrawable=%d dropped=%d qDrop=%d qDry=%d maxRenderMs=%.1f "
                + "inflightMax=%d forced=%d direct=%d scanSwitch=%d "
                + "glassDeltaMs p50=%.2f max=%.2f n=%d "
                + "latchMs p50=%.2f max=%.2f vendLeadMs p50=%.2f max=%.2f",
            pace(), linkPeriod() * 1000,
            decoded, ok, failed, empty, gated, noDrawable, dropped,
            smoothing.overflowDrops, smoothing.underflows, maxRenderMs, inflightMax,
            gate?.drainForced() ?? 0, directPresents, scanoutSwitches(),
            p50, dMax, deltas.count, latchP50, latchMax,
            vendP50, vendMax) + glassLine + cadenceLine
        directPresents = 0
        decoded = 0; decodedRepeats = 0; shownRepeats = 0
        ok = 0; failed = 0; empty = 0; dropped = 0; gated = 0; noDrawable = 0
        maxRenderMs = 0
        maxInFlight = inFlight // the window peak restarts from the live depth
        glassDeltasMs.removeAll(keepingCapacity: true)
        cadenceErrMs.removeAll(keepingCapacity: true)
        steps.removeAll(keepingCapacity: true)
        displayMs.removeAll(keepingCapacity: true)
        latchMs.removeAll(keepingCapacity: true)
        vendLeadMs.removeAll(keepingCapacity: true)
        lock.unlock()
        // Console.app first (the on-device readout — see presentLog); stdout only under the env
        // lever (the CLI client's capture channel).
        presentLog.info("\(line, privacy: .public)")
        if presentDebug {
            print(line)
            fflush(stdout) // stdout is a pipe when captured — flush per line or nothing shows
        }
    }
}

#endif
