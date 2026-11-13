// The present cadence: what a steady rate looks like, how far a frame may sit from the
// grid before it counts as late, and the running health that answers it. Arithmetic over
// timestamps, with no dependency on Stage2Pipeline.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import Metal
import PunktfunkShared
import QuartzCore
import os

/// Tuning for one cadence loop. Gains are SHIFT COUNTS — the loop is fixed-point Int64
/// throughout, so it runs identically on every client and in the offline harness, and carries no
/// float into a present path.
///
/// The values are derived from first principles, not fitted to recorded `(src_pts, received,
/// decoded)` traces: a proportional time constant of tens of frames, an integral an order
/// slower, a cushion of a few mean-absolute-deviations.
///
/// ⚠ A hand-written port of `punktfunk_core::phase::CadenceTuning`, exactly as `AudioRing` is a
/// port of `JitterPolicy`: this pipeline is Swift and does not link that Rust type. **Every
/// constant and rule here must stay in lockstep with it** — `CadenceClockTests` and the Rust
/// `phase::tests` assert the two against the same synthetic input, and `CadenceVectorTests`
/// replays the runs `phase::vectors` wrote to `clients/shared/cadence-vectors.json`. That
/// agreement IS the contract.
struct CadenceTuning: Equatable {
    /// Proportional gain on the offset estimate: `1 >> offsetShift` of the residual per frame.
    var offsetShift: UInt8
    /// Integral gain on the per-frame rate (skew) term: `1 >> skewShift`.
    var skewShift: UInt8
    /// EMA weight for the residual mean-absolute-deviation.
    var jitterShift: UInt8
    /// Per-sample residual clamp — one outlier must not yank the estimate.
    var errorClampNs: Int64
    /// Cushion = `mad * cushionNum / cushionDen`, clamped to
    /// `[cushionFloorNs, frameIntervalNs]`.
    var cushionNum: UInt16
    var cushionDen: UInt16
    var cushionFloorNs: Int64
    /// Source-timestamp gap beyond which the loop re-anchors instead of tracking.
    var reanchorGapNs: Int64

    /// For callers that snap the due time onto a display grid afterwards: the snap-up itself
    /// carries roughly half a refresh of implicit slack, so the cushion can be small. Every Apple
    /// present snaps — onto `VsyncClock.nextVsync` under arrival/glass pacing, onto the
    /// CAMetalDisplayLink's own vend under deadline pacing — so this is the one this client runs.
    static func snapping() -> CadenceTuning {
        CadenceTuning(
            offsetShift: 5, skewShift: 10, jitterShift: 5, errorClampNs: 20_000_000,
            cushionNum: 2, cushionDen: 1, cushionFloorNs: 500_000,
            reanchorGapNs: 500_000_000)
    }

    /// For callers presenting at the due time directly (VRR, direct scanout): no implicit slack,
    /// so the cushion must cover more of the distribution on its own.
    static func freeRunning() -> CadenceTuning {
        var t = snapping()
        t.cushionNum = 3
        t.cushionFloorNs = 2_000_000
        return t
    }
}

/// Loop health for the pf-present line — the numbers that say whether the cushion is doing its
/// job. Mirrors `punktfunk_core::phase::CadenceHealth`.
///
/// Residual PERCENTILES are deliberately absent: this type holds no histogram, and the client
/// stat paths (the latency meters) are where distributions belong.
struct CadenceHealth: Equatable {
    /// Frames folded since the last `reset`.
    var frames: UInt64 = 0
    /// …of which the due time was already past when the frame became presentable. The direct
    /// signal that the cushion is too small.
    var late: UInt64 = 0
    /// Times the loop gave up tracking and re-anchored (gap, regression, or explicit reset).
    var reanchors: UInt64 = 0
    var offsetNs: Int64 = 0
    var skewNs: Int64 = 0
    var jitterNs: Int64 = 0
    var cushionNs: Int64 = 0
}

/// Plays frames out on the SOURCE's cadence instead of on their arrival instant, so transport
/// jitter — and a raggedly-delivering host compositor — does not land on the glass 1:1.
///
/// Type-2: it tracks offset AND per-frame rate, because two free-running crystals produce a ramp
/// and a proportional-only loop lags a ramp forever.
///
/// It smooths the OFFSET, never the timestamps. Due is `srcPts + offset + cushion`, so genuine
/// variation in the source's cadence passes straight through and only the transport's share of
/// `ready − pts` is filtered; more evenly spaced due times than the source would be a bug.
///
/// One clock domain throughout: a constant offset between domains is absorbed by the estimator,
/// so the caller converts the decode instant to `CACurrentMediaTime` once on the way in and the
/// due time needs none. Suspend/resume breaks the constant — the gap re-anchor covers it.
///
/// A late frame's due time comes back in the PAST, unclamped: clamping to `readyNs` would turn
/// every late frame into a fresh anchor, which is the arrival-driven presentation this replaces.
///
/// ⚠ A hand-written port of `punktfunk_core::phase::CadenceClock` — see `CadenceTuning` for the
/// lockstep contract. Sendable; lock-guarded — the decode-completion thread folds frames while the
/// render thread reads health.
final class CadenceClock: @unchecked Sendable {
    private let lock = NSLock()
    private let tuning: CadenceTuning
    /// `ready − srcPts`, smoothed. Absorbs the clock-domain constant.
    private var offsetNs: Int64 = 0
    /// Per-frame drift of that offset — the integral term.
    private var skewNs: Int64 = 0
    /// EMA of |residual|, the cushion's input.
    private var madNs: Int64 = 0
    /// nil until the first sample anchors the loop.
    private var lastPtsNs: UInt64?
    /// Last frame interval seen, so `cushionNs` can apply its ceiling.
    private var frameIntervalNs: Int64 = 0
    private var counters = CadenceHealth()

    init(tuning: CadenceTuning) {
        self.tuning = tuning
    }

    /// Force a re-anchor on the next sample. Call on every discontinuity the client already knows
    /// about: reanchor, codec rebuild, surface recreate, jump-to-live, resume.
    func reset() {
        lock.lock()
        lastPtsNs = nil
        skewNs = 0
        // `madNs` deliberately SURVIVES. It describes the link, not the stream, and a cushion that
        // collapsed to its floor at every rebuild would spend the next few hundred frames
        // presenting late — the exact failure the cushion exists to prevent.
        lock.unlock()
    }

    /// Fold one presentable frame and return when it is due, in the present clock domain.
    ///
    /// `readyNs` is when the frame became presentable; `frameIntervalNs` is the nominal source
    /// interval and the cushion's ceiling.
    ///
    /// The result **may be earlier than `readyNs`** — that is a late frame, and the caller's
    /// contract is "already due ⇒ present at the next opportunity", never "drag the grid back to
    /// now".
    func dueNs(srcPtsNs: UInt64, readyNs: Int64, frameIntervalNs: Int64) -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        self.frameIntervalNs = frameIntervalNs
        counters.frames += 1
        let pts = Int64(bitPattern: srcPtsNs)
        let raw = saturatingSub(readyNs, pts)

        let anchored: Bool
        if let last = lastPtsNs {
            // Source time going BACKWARDS, or a gap so long the estimate cannot be trusted to
            // have tracked across it: re-anchor rather than slew for seconds.
            anchored =
                !(srcPtsNs < last
                    || srcPtsNs - last > UInt64(bitPattern: tuning.reanchorGapNs))
        } else {
            anchored = false
        }
        if anchored {
            // Advance the estimate one frame on the rate term, then correct it by a bounded
            // fraction of what the new sample says.
            offsetNs = saturatingAdd(offsetNs, skewNs)
            let err = min(
                max(saturatingSub(raw, offsetNs), -tuning.errorClampNs), tuning.errorClampNs)
            offsetNs = saturatingAdd(offsetNs, shrTowardZero(err, tuning.offsetShift))
            skewNs = saturatingAdd(skewNs, shrTowardZero(err, tuning.skewShift))
            let dev = abs(err) - madNs
            madNs = saturatingAdd(madNs, shrTowardZero(dev, tuning.jitterShift))
        } else {
            offsetNs = raw
            skewNs = 0
            counters.reanchors += 1
        }
        lastPtsNs = srcPtsNs

        let due = saturatingAdd(saturatingAdd(pts, offsetNs), lockedCushionNs())
        if due < readyNs { counters.late += 1 }
        return due
    }

    /// A frame whose timestamp is not on the source cadence — a repeat the host re-anchored at
    /// submit, a stamp its plausibility gate replaced with "now", or one that reached us with no
    /// usable pts at all. Those samples do not lie on the source's timeline, and folding them in
    /// would drag the offset estimate toward "now" exactly when the stream is idle and the
    /// estimate matters most.
    ///
    /// Returns a due time from the CURRENT estimate, leaving offset, skew and jitter untouched:
    /// the frame is simply due once it is ready, cushioned like any other.
    func noteOffCadence(readyNs: Int64, frameIntervalNs: Int64) -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        self.frameIntervalNs = frameIntervalNs
        return saturatingAdd(readyNs, lockedCushionNs())
    }

    func jitterNs() -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        return madNs
    }

    /// How far past the estimate a frame is held, to absorb the measured jitter.
    ///
    /// The one-frame-interval ceiling is an INVARIANT, not a tunable: a cushion past a whole frame
    /// buys latency for smoothness the source cannot supply, and at that point the honest fix is a
    /// deeper buffer the user asked for, not a loop quietly holding frames.
    func cushionNs() -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        return lockedCushionNs()
    }

    func health() -> CadenceHealth {
        lock.lock()
        defer { lock.unlock() }
        var h = counters
        h.offsetNs = offsetNs
        h.skewNs = skewNs
        h.jitterNs = madNs
        h.cushionNs = lockedCushionNs()
        return h
    }

    private func lockedCushionNs() -> Int64 {
        let den = Int64(max(tuning.cushionDen, 1))
        let want = saturatingMul(madNs, Int64(tuning.cushionNum)) / den
        let ceiling = frameIntervalNs > 0 ? frameIntervalNs : Int64.max
        return min(max(want, min(tuning.cushionFloorNs, ceiling)), ceiling)
    }
}

/// Arithmetic shift that rounds toward ZERO, so a negative residual is damped by exactly as much
/// as its positive twin. A plain `>>` rounds toward −∞, which biases a loop that spends its whole
/// life within a few nanoseconds of zero error.
private func shrTowardZero(_ v: Int64, _ shift: UInt8) -> Int64 {
    v < 0 ? -((-v) >> shift) : v >> shift
}

/// The Rust loop is `saturating_*` throughout — a garbage timestamp must clamp the estimate, never
/// trap the render thread. Swift's operators trap instead of saturating, so the port carries its
/// own; for every reachable input these are plain `+`, `−`, `×`.
private func saturatingAdd(_ a: Int64, _ b: Int64) -> Int64 {
    let (v, overflow) = a.addingReportingOverflow(b)
    return overflow ? (b > 0 ? Int64.max : Int64.min) : v
}

private func saturatingSub(_ a: Int64, _ b: Int64) -> Int64 {
    let (v, overflow) = a.subtractingReportingOverflow(b)
    return overflow ? (b > 0 ? Int64.min : Int64.max) : v
}

private func saturatingMul(_ a: Int64, _ b: Int64) -> Int64 {
    let (v, overflow) = a.multipliedReportingOverflow(by: b)
    guard overflow else { return v }
    return (a > 0) == (b > 0) ? Int64.max : Int64.min
}
#endif
