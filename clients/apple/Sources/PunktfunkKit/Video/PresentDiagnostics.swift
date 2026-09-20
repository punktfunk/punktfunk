// Present-path diagnostics: the once-a-second pacing line, the display link's advertised
// timing, and the phase reporter behind PUNKTFUNK_PRESENT_DEBUG. None of it participates
// in presenting a frame — it only watches — so it lives beside the pipeline, not in it.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import Metal
import PunktfunkShared
import QuartzCore
import os

/// The pf-present line's os_log mirror — same category as the presenter's own.
private let presentLog = ClientLog(category: "present")

/// The deadline link's frame-latency ASK and property READBACK, published for the HUD to render.
///
/// ⚠ A readback is NOT a grant. `preferredFrameLatency` is a plain read-write float
/// (CAMetalDisplayLink.h carries no doc contract), so reading it returns whatever we last
/// stored unless the system actively clamps the setter — and the 2026-08-13 field run proved
/// how misleading that is: it read 1.00 while the measured vend lead sat at 1.95 refresh
/// periods. The number that tells the truth about scheduling is the vend lead (the HUD's
/// `os present` floor), never this property. The line still earns its place twice over: a
/// readback that DIFFERS from the ask is the one clamp signal the API can give, and the ask
/// must be visible on screen because **on tvOS no log is reachable** — `log stream --device`
/// is gone from modern macOS, `log collect --device-name` needs root and then fails "Device
/// not configured" because an Apple TV has no USB to fall back to, and the libimobiledevice
/// pairing is a different database from Xcode's. Console.app is a GUI.
///
/// A process-global rather than a sixth parameter threaded through SessionModel → StreamView →
/// controller → SessionPresenter → Stage2Pipeline → delegate: it is write-once-per-session
/// diagnostics, and this file already keeps `presentDebug`/`presentLog` at file scope. Reset by
/// `clear()` at session start so a stale session's answer can never be read as this one's.
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

/// Deadline pacing's staged frame-rate hint, and — on every pacing — the session's nominal source
/// interval (`sourceIntervalNs`). SessionPresenter pushes the stream rate from the
/// MAIN thread (session start + every layout/Reconfigure); the link's own thread drains and
/// applies it, so the CAMetalDisplayLink is only ever touched from the thread that runs it. The
/// floor is PINNED at the stream rate — no idle ramp-down: with a low floor the link idles toward
/// it on a static scene (infinite GOP ⇒ no frames), and the first damage frame after idle would
/// wait out a slow tick before it could present. Empty wakes at stream rate are near-free; the
/// PANEL still idles via VRR because no presents happen. Sendable; lock-guarded.
final class FrameRateHint: @unchecked Sendable {
    private let lock = NSLock()
    private var pending: CAFrameRateRange?
    private var streamHz: Float = 0
    private var boosted = false
    func stage(hz: Float) {
        guard hz > 0 else { return }
        lock.lock()
        streamHz = hz
        pending = Self.range(hz: hz, boosted: boosted)
        lock.unlock()
    }
    /// Pen-proximity boost: pin `minimum = preferred` at the range's CEILING instead of the
    /// stream rate. UIKit delivers touch/Pencil events at the PANEL's cadence, and the panel
    /// follows this link's vote — so a 60 fps stream on a 120 Hz iPad halves pencil sampling
    /// unless a boost lifts the panel while the Pencil is in range. Presents still pace at
    /// stream rate (extra link updates just vend into the newest-wins stash), so the cost is
    /// empty wakes, scoped to pen proximity.
    func setBoost(_ on: Bool) {
        lock.lock()
        if boosted != on {
            boosted = on
            if streamHz > 0 { pending = Self.range(hz: streamHz, boosted: on) }
        }
        lock.unlock()
    }
    func drain() -> CAFrameRateRange? {
        lock.lock()
        defer { lock.unlock() }
        let p = pending
        pending = nil
        return p
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
    private static func range(hz: Float, boosted: Bool) -> CAFrameRateRange {
        #if os(tvOS)
        // A TV is fixed-rate, so all three bounds pin to the stream rate: a range is a promise
        // about how variable our cadence may be, and a scheduler handed 60…120 on a 60 Hz panel
        // keeps a refresh of slack in hand. `boosted` is for pen proximity, which tvOS lacks.
        _ = boosted
        return CAFrameRateRange(minimum: hz, maximum: hz, preferred: hz)
        #else
        let cap = max(hz, 120)
        let preferred = boosted ? cap : hz
        return CAFrameRateRange(minimum: preferred, maximum: cap, preferred: preferred)
        #endif
    }
}

/// The client half of phase-locked capture (design/phase-locked-capture.md): the decode
/// callback deposits per-AU arrival stamps (client CLOCK_REALTIME — the core's reassembly-
/// completion time), the deadline link's thread deposits the latch grid, and ~1 Hz that same
/// thread flushes the circular arrival-phase statistic to the host. The statistic is a
/// verbatim port of `punktfunk_core::phase::circular_latch` — the host's v3 controller
/// (grid-locked submits, coherence-gated engage) was tuned against exactly it, and a
/// period-smeared Wi-Fi link correctly reads coherence ≈ 0 there, so the controller never
/// engages where alignment is physically pointless. Shared box (never captures the pipeline);
/// a session's connection binds/unbinds like DecodeReport/KeyframeRecovery.
final class PhaseReporter: @unchecked Sendable {
    private let lock = NSLock()
    private var connection: PunktfunkConnection?
    /// Arrival stamps since the last flush, client CLOCK_REALTIME. Bounded: ~1 s at 240 fps.
    private var arrivalsNs: [Int64] = []
    /// Smallest update-to-update spacing this window: successive `nextLatch` values sit one
    /// panel period apart except across skipped link updates (2×, 3×, …), so the window
    /// minimum IS the period. Re-learned every flush so VRR/mode switches track both ways.
    private var periodNs: Int64 = 0
    private var prevLatchRealNs: Int64 = 0
    private var lastFlushRealNs: Int64 = 0

    func bind(_ c: PunktfunkConnection?) {
        lock.lock()
        connection = c
        arrivalsNs.removeAll()
        periodNs = 0
        prevLatchRealNs = 0
        lastFlushRealNs = 0
        lock.unlock()
    }

    /// Decode-callback side: one AU's arrival (reassembly-completion) stamp.
    func noteArrival(receivedNs: Int64) {
        lock.lock()
        if connection != nil, arrivalsNs.count < 256 { arrivalsNs.append(receivedNs) }
        lock.unlock()
    }

    /// Link-thread side, once per update: where the NEXT latch sits on the client's realtime
    /// clock (the arrival stamps' domain). Learns the period from update spacing and ~1 Hz
    /// converts the window's arrivals into leads against this grid, then reports — a
    /// fire-and-forget datagram push on the control plane.
    func noteGrid(nextLatchRealNs: Int64) {
        lock.lock()
        if prevLatchRealNs > 0 {
            let delta = nextLatchRealNs - prevLatchRealNs
            // 2–100 ms accepts 10–500 Hz panels, rejects wakeup hiccups and clock jumps.
            if delta > 2_000_000, delta < 100_000_000, periodNs == 0 || delta < periodNs {
                periodNs = delta
            }
        }
        prevLatchRealNs = nextLatchRealNs
        guard let c = connection, periodNs > 0, arrivalsNs.count >= 8,
            nextLatchRealNs - lastFlushRealNs >= 1_000_000_000
        else {
            lock.unlock()
            return
        }
        lastFlushRealNs = nextLatchRealNs
        let period = periodNs
        let leadsUs = arrivalsNs.map { a -> UInt64 in
            let m = (nextLatchRealNs - a) % period
            return UInt64(m < 0 ? m + period : m) / 1000
        }
        arrivalsNs.removeAll(keepingCapacity: true)
        periodNs = 0
        let offsetNs = c.clockOffsetNs
        lock.unlock()
        guard
            let (leadMeanNs, coherence) = Self.circularLatch(
                samplesUs: leadsUs, periodNs: period)
        else { return }
        c.reportPhase(
            nextLatchHostNs: UInt64(max(0, nextLatchRealNs + offsetNs)),
            latchPeriodNs: UInt32(clamping: period),
            uncertaintyNs: 1_000_000, // skew residual — same conservative 1 ms as Android
            arrivalLeadNs: UInt32(clamping: leadMeanNs),
            coherenceMilli: coherence)
    }

    /// Verbatim port of `punktfunk_core::phase::circular_latch` (µs samples against an ns
    /// period; nil under 8 samples). The MEAN is what a phase controller can steer under
    /// jitter — a period-spanning distribution's median is immovable — and the coherence
    /// (resultant length, ‰) says whether any phase exists to steer at all.
    static func circularLatch(samplesUs: [UInt64], periodNs: Int64) -> (UInt64, UInt16)? {
        guard samplesUs.count >= 8, periodNs > 0 else { return nil }
        let periodUs = Double(periodNs) / 1000.0
        var x = 0.0
        var y = 0.0
        for s in samplesUs {
            let theta = Double(s).truncatingRemainder(dividingBy: periodUs) / periodUs * 2 * .pi
            x += cos(theta)
            y += sin(theta)
        }
        let n = Double(samplesUs.count)
        let r = (x * x + y * y).squareRoot() / n
        var meanTheta = atan2(y, x)
        if meanTheta < 0 { meanTheta += 2 * .pi }
        return (UInt64(meanTheta / (2 * .pi) * Double(periodNs)), UInt16(r * 1000.0))
    }
}

/// PUNKTFUNK_PRESENT_DEBUG=1 aggregation: one printed line per second from the render thread with
/// the decode rate, render outcomes, the slowest render call (≈ nextDrawable wait) and the deltas
/// between system-reported on-glass times (vsync-aligned presents show clean refresh-period
/// multiples; immediate flips scatter). Lock-guarded — `presented` lands on a Metal callback thread.
final class PresentDebugStats: @unchecked Sendable {
    /// The session's cadence loop, for the line's `cadence` segment — `nil` under the latency
    /// intent, and then the line is emitted exactly as it was before source-timestamp playout
    /// existed. `late` is the number WP8 gates on: a due time already past when the frame became
    /// presentable is the direct signal that the cushion is too small.
    private let cadence: CadenceClock?
    private let lock = NSLock()
    private var last = CACurrentMediaTime()
    private var ok = 0, failed = 0, empty = 0, dropped = 0, gated = 0, noDrawable = 0
    private var maxRenderMs = 0.0
    private var lastGlassNs: Int64 = 0
    private var glassDeltasMs: [Double] = []
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
    /// The session's pace name for the line's `pace=` field — a closure because the macOS
    /// "(composited)" suffix tracks the windowed present routing live.
    private let pace: () -> String
    /// The ordinary link's last reported period in seconds, for `linkMs` — 0 before the first
    /// tick and under deadline pacing (no ordinary link).
    private let linkPeriod: () -> CFTimeInterval

    init(
        cadence: CadenceClock?, pace: @escaping () -> String,
        linkPeriod: @escaping () -> CFTimeInterval
    ) {
        self.cadence = cadence
        self.pace = pace
        self.linkPeriod = linkPeriod
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

    func presented(atNs: Int64?, issuedNs: Int64) {
        lock.lock()
        inFlight = max(0, inFlight - 1) // clamp: the handler can beat renderReturned's increment
        if let atNs {
            if lastGlassNs > 0 { glassDeltasMs.append(Double(atNs - lastGlassNs) / 1e6) }
            lastGlassNs = atNs
            latchMs.append(Double(atNs - issuedNs) / 1e6)
        } else {
            dropped += 1
        }
        lock.unlock()
    }

    func flushIfDue(ring: FrameStore<ReadyFrame>, gate: PresentGate?) {
        lock.lock()
        let now = CACurrentMediaTime()
        guard now - last >= 1 else { lock.unlock(); return }
        last = now
        let decoded = ring.drainSubmitted()
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
        // Loop health, appended only where a loop exists — `late`/`frames` is WP8's cushion
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
                + "inflightMax=%d forced=%d glassDeltaMs p50=%.2f max=%.2f n=%d "
                + "latchMs p50=%.2f max=%.2f vendLeadMs p50=%.2f max=%.2f",
            pace(), linkPeriod() * 1000,
            decoded, ok, failed, empty, gated, noDrawable, dropped,
            smoothing.overflowDrops, smoothing.underflows, maxRenderMs, inflightMax,
            gate?.drainForced() ?? 0, p50, dMax, deltas.count, latchP50, latchMax,
            vendP50, vendMax) + cadenceLine
        ok = 0; failed = 0; empty = 0; dropped = 0; gated = 0; noDrawable = 0
        maxRenderMs = 0
        maxInFlight = inFlight // the window peak restarts from the live depth
        glassDeltasMs.removeAll(keepingCapacity: true)
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
