import XCTest

#if canImport(Metal)
@testable import PunktfunkKit

/// Deadline pacing replayed on the real `DeadlinePacer`, `DeadlineSelector` (`PhaseTracker` and
/// `Carry` inside) and `FrameStore`. The render thread's pairing and hold restate
/// `startDeadlinePresenter` and `holdForPick`; the clock, the panel and the GPU are models: one
/// drawable per refresh at k·P, deadline = vend + P − 1.5 ms, glass = vend + 2P, issue → GPU done
/// 1.5 ms, decode times Gaussian around a fixed phase.
private struct ReplayRng {
    var s: UInt64
    init(_ seed: UInt64) { s = seed }
    mutating func next() -> UInt64 {
        s &+= 0x9E37_79B9_7F4A_7C15
        var z = s
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        return z ^ (z >> 31)
    }
    mutating func uniform() -> Double { Double(next() >> 11) / 9_007_199_254_740_992.0 }
    mutating func gauss() -> Double {
        let u1 = max(uniform(), 1e-12), u2 = uniform()
        return (-2 * log(u1)).squareRoot() * cos(2 * .pi * u2)
    }
}

private struct ReplayResult {
    var drops = 0, repeats = 0
    /// The longest run of picks that kept a frame, and of consecutive shown frames a refresh old.
    var longestRun = 0, longestQueue = 0
    /// The largest carry bound the selector reported.
    var maxBound = 0
}

private func ns(_ t: Double) -> Int64 { Int64((t * 1e9).rounded()) }

/// The first 3 s are not counted. `hz` is the panel, `hostHz` the stream, `phase` the first
/// decode's offset after vend 0 and `sigma` the decode jitter, both in seconds.
private func replay(
    hz: Double, hostHz: Double, phase: Double, sigma: Double, duration: Double
) -> ReplayResult {
    let P = 1 / hz
    let statsFrom = 3.0
    var rng = ReplayRng(1)
    var arrivals: [Double] = []
    while true {
        var a = phase + Double(arrivals.count) / hostHz + sigma * rng.gauss()
        a = max(a, (arrivals.last ?? -1) + 0.0003) // the decoder is serial
        if a > duration + 1 { break }
        arrivals.append(a)
    }
    let ring = FrameStore<Int>(policy: .newestTwo)
    var next = 0
    func submit(upTo t: Double) {
        while next < arrivals.count, arrivals[next] <= t {
            ring.submit(next)
            next += 1
        }
    }
    var selector = DeadlineSelector(latePick: true, now: 0)
    let pacer = DeadlinePacer()
    var pending: [(t: Double, issuedNs: Int64, presentedNs: Int64, targetNs: Int64, leadNs: Int64)] = []
    func flush(upTo t: Double) {
        while let p = pending.first, p.t <= t {
            pending.removeFirst()
            pacer.observe(
                issuedNs: p.issuedNs, presentedNs: p.presentedNs, targetNs: p.targetNs,
                cutoffLeadNs: p.leadNs)
        }
    }
    var r = ReplayResult()
    var vendShown: [Int?] = []
    var run = 0, queue = 0
    var nextLine = 1.0
    let vends = Int(duration / P)
    for k in 0..<vends {
        let V = Double(k) * P
        flush(upTo: V)
        if V >= nextLine {
            // The render loop's once-a-second line ends both windows.
            _ = pacer.windowLine()
            let tail = selector.endWindow(report: true, composition: { "na" }) ?? ""
            if V >= statsFrom,
                let bound = tail.split(separator: " ").first(where: { $0.hasPrefix("carryBound=") })
                    .flatMap({ Int($0.dropFirst("carryBound=".count)) })
            {
                r.maxBound = max(r.maxBound, bound)
            }
            nextLine += 1
        }
        let deadline = V + P - 0.0015, target = V + 2 * P
        let last = DeadlinePacer.cutoff(vendAt: V, deadline: deadline, margin: pacer.margin)
        submit(upTo: V)
        var tPair = V
        var taken = ring.take()
        if taken == nil, next < arrivals.count, arrivals[next] <= last {
            tPair = arrivals[next]
            flush(upTo: tPair)
            submit(upTo: tPair)
            taken = ring.take()
        }
        guard let frame = taken else {
            vendShown.append(nil)
            continue
        }
        let drain = pacer.takeDrain(nowNs: ns(tPair))
        let pairing = selector.pair(vendAt: V, period: P, last: last, now: tPair, drain: drain)
        guard case .present(let at, let switched) = pairing else {
            ring.putBack(frame)
            vendShown.append(nil)
            continue
        }
        // holdForPick: wait for the pick, gather what arrived, show one and keep one.
        let waited = at > tPair
        let tPick = waited ? at + 0.00005 + 0.0002 * rng.uniform() : tPair
        flush(upTo: tPick)
        submit(upTo: tPick)
        selector.noteRate(at: tPick, period: P, submitted: { ring.submittedTotal() })
        let frames = [frame] + ring.takeAll()
        let choice = selector.choose(frames, switched: switched)
        if let keep = choice.keep { ring.putBack(keep) }
        if waited { pacer.noteHeld(wakeLate: tPick - at, superseded: frames.count > 1) }
        run = choice.keep != nil ? run + 1 : 0
        let show = choice.show
        selector.noteShown(decodedNs: ns(arrivals[show]), period: P) { ns(V) }
        let end = tPick + 0.0015
        pacer.noteGPU(issuedAt: tPick, start: tPick + 0.0002, end: end, deadline: deadline, period: P)
        pacer.noteRender(seconds: 0.0003)
        pacer.noteIssued(ns: ns(tPick))
        let glass = end > deadline ? target + P : target
        pending.append((
            t: glass + 0.0005, issuedNs: ns(tPick), presentedNs: ns(glass), targetNs: ns(target),
            leadNs: ns(target - last)))
        pending.sort { $0.t < $1.t }
        vendShown.append(show)
        if V >= statsFrom {
            r.longestRun = max(r.longestRun, run)
            if (tPick - arrivals[show]) / P >= 1 {
                queue += 1
                r.longestQueue = max(r.longestQueue, queue)
            } else {
                queue = 0
            }
        }
    }
    // Drops and repeats over the stats span.
    let shown = Set(vendShown.compactMap { $0 })
    let endT = Double(vends) * P - 3 * P
    r.drops = arrivals.indices.filter {
        arrivals[$0] >= statsFrom && arrivals[$0] < endT && !shown.contains($0)
    }.count
    let nominal = max(1, Int((hz / hostHz).rounded()))
    var lastVend: Int?
    for k in Int((statsFrom / P).rounded(.up))..<vendShown.count where vendShown[k] != nil {
        if let l = lastVend, k - l > nominal { r.repeats += k - l - nominal }
        lastVend = k
    }
    return r
}

final class CarryReplayTests: XCTestCase {
    /// A true 120.000 Hz host under a 120 Hz panel, decode jitter σ 2 ms, 120 s. A 2 s rate
    /// count read it 0.5 Hz slow in about 5 % of windows. Taken for a slow host, that lifted the
    /// carry's bound to 240 picks and queues of 105 stood (longest run 107, 195 drops). One frame
    /// over the 8 s window is 0.125 Hz, so the bound stays 24 (longest run 24, queue 38, 197 drops).
    func testRateCountNoiseKeepsTheCarryBound() {
        let r = replay(hz: 120, hostHz: 120, phase: 0.0005, sigma: 0.002, duration: 120)
        XCTAssertEqual(r.maxBound, Carry.maxRun, "count noise is not a slow host")
        XCTAssertLessThanOrEqual(r.longestRun, Carry.maxRun)
        XCTAssertLessThanOrEqual(r.longestQueue, 2 * Carry.maxRun)
        XCTAssertLessThanOrEqual(r.drops, 200, "the shorter queue costs no frames")
    }

    /// A host 0.6 Hz slower than a 120 Hz panel, σ 1 ms, 120 s: a real slow host, whose own gap
    /// each beat ends the carry's holds. The bound lifts to a beat and no frame drops; a 0.8 Hz
    /// tolerance kept it at 24 and dropped 11.
    func testASlowHostKeepsTheBeatBound() {
        let r = replay(hz: 120, hostHz: 119.4, phase: 0.0064, sigma: 0.001, duration: 120)
        XCTAssertGreaterThan(r.maxBound, Carry.maxRun, "measurably slow: one beat")
        XCTAssertGreaterThan(r.longestRun, Carry.maxRun)
        XCTAssertEqual(r.drops, 0, "the beat's own gap ends each hold")
    }
}
#endif
