// Per-frame latency sampler: records one interval per frame (an end instant minus a start
// instant, both CLOCK_REALTIME ns) and drains percentiles on demand.
// NSLock rather than an actor — the writers are the non-async pump/decode/present paths (same
// pattern as the app's FrameMeter).

import Foundation

/// Samples one latency interval per frame and reports percentiles. The stats overlay's figures
/// live in the core; this serves the app's own readers: capture→on-glass for the A/V sync loop,
/// the client-queue wait, and the stage-1 pump.
///
/// For a host-anchored interval (capture→…) the sample is `end + offset - pts_ns`, where `pts_ns`
/// is the host's capture wall clock (the AU's pts) and the LIVE **clock-skew offset**
/// (`PunktfunkConnection.clockOffsetNs`, host minus client, mid-stream re-synced — read it per
/// record, never cached) makes the difference valid across machines. A client-local interval
/// passes its start stamp as `ptsNs` and `offsetNs: 0`.
public final class LatencyMeter: @unchecked Sendable {
    private let lock = NSLock()
    private var samplesUs: [Int64] = []
    /// Window cap: 17 s at 240 Hz, far past a 1 Hz drain. A meter read only through
    /// `latestSample` is never drained, and must not grow for the whole session.
    private static let maxSamples = 4096
    private var skewCorrected = false
    /// The most recent sample and the instant it ended, for `latestSample(asOfNs:maxAgeMs:)` —
    /// a LEVEL, not a window, so `drain` deliberately leaves both alone.
    private var latestNs: Int64 = 0
    private var latestAtNs: Int64 = 0

    public init() {}

    /// Record one frame at receipt (now). `ptsNs` is the host capture clock (the AU's pts);
    /// `offsetNs` is the host-client clock offset from the skew handshake (0 = uncorrected).
    public func record(ptsNs: UInt64, offsetNs: Int64) {
        let nowNs = realtimeNowNs()
        record(ptsNs: ptsNs, atNs: nowNs, offsetNs: offsetNs)
    }

    /// Record one frame whose sample is `atNs + offsetNs - ptsNs` — an EXPLICIT end instant
    /// rather than now. `ptsNs` is the stage's start point: the AU pts for the host-anchored
    /// intervals, or a client stamp (receivedNs / decodedNs, with `offsetNs: 0`) for the local
    /// decode/display stages. The stage-2 presenter stamps its present-side samples at the
    /// display link's target present time (not the moment the present call ran). All in
    /// `CLOCK_REALTIME`.
    public func record(ptsNs: UInt64, atNs: Int64, offsetNs: Int64) {
        let latNs = atNs &+ offsetNs &- Int64(bitPattern: ptsNs)
        // Samples outside (0, 10 s) are a clock step, a wrong offset or a missing start stamp. The
        // overlay's clock-suspect warning is the core's; here they are only dropped.
        guard latNs > 0, latNs < 10_000_000_000 else { return }
        lock.lock()
        if samplesUs.count < Self.maxSamples { samplesUs.append(latNs / 1000) }
        latestNs = latNs
        latestAtNs = atNs
        if offsetNs != 0 { skewCorrected = true }
        lock.unlock()
    }

    /// The most recent single sample in ns, or `nil` if none has landed or the last one ended more
    /// than `maxAgeMs` before `nowNs` (both `CLOCK_REALTIME`). Unlike `drain`, this reports a level
    /// rather than a window, and reading it consumes nothing.
    ///
    /// **What it is for.** Read off the END-TO-END meter, this is the video plane's live
    /// glass-to-glass figure — `displayed + clockOffset − pts`, exactly the shape `AvSync` compares
    /// audio against — and it is the reference the A/V sync loop needs. It is published from
    /// `record`, so every presenter feeds it (stage-1 at enqueue, the others on glass) without
    /// knowing that audio exists.
    ///
    /// **Why staleness is not optional.** The number is a level, so absent an age check it would
    /// simply keep its last value forever. This client has a state where that matters: the
    /// backgrounded keep-alive keeps audio playing and DROPS video decode entirely, so the loop
    /// would go on steering the ring against a reference minutes old and frozen. Expiring it
    /// returns `nil`, which is the same "no reference yet" case as session start — the loop holds
    /// its last correction and stops chasing. `nowNs` is caller-supplied rather than read fresh so
    /// the audio side compares against exactly the instant it timestamped its own frame at.
    ///
    /// Only the PAST is bounded. A present stamp can legitimately sit a hair ahead of the reader's
    /// clock (the deadline presenter stamps at the link's target present time), and discarding the
    /// only reference we have over a fraction of a refresh would make it flap in and out; a stamp
    /// wildly in the future instead yields a huge offset, which `AvSync` refuses on its own terms.
    public func latestSample(asOfNs nowNs: Int64, maxAgeMs: Int) -> Int64? {
        lock.lock()
        defer { lock.unlock() }
        guard latestNs > 0 else { return nil }
        guard (nowNs &- latestAtNs) <= Int64(maxAgeMs) * 1_000_000 else { return nil }
        return latestNs
    }

    public struct Stats: Sendable {
        public let p50Ms: Double
        public let p95Ms: Double
        public let p99Ms: Double
        public let count: Int
        /// True if the skew offset was applied (a host that answered the handshake) — i.e. the
        /// numbers are cross-machine valid, not just same-host.
        public let skewCorrected: Bool
    }

    /// Forget everything, level included. A meter outlives the session that fed it, and a pump can
    /// still deliver a frame after `disconnect`, so a new session would otherwise read the previous
    /// one's samples in its first window.
    public func reset() {
        lock.lock()
        defer { lock.unlock() }
        samplesUs.removeAll(keepingCapacity: true)
        skewCorrected = false
        latestNs = 0
        latestAtNs = 0
    }

    /// Percentiles over the samples accumulated since the last drain, then reset the window. `nil`
    /// when no samples arrived in the interval.
    public func drain() -> Stats? {
        lock.lock()
        let sorted = samplesUs.sorted()
        let corrected = skewCorrected
        samplesUs.removeAll(keepingCapacity: true)
        skewCorrected = false
        lock.unlock()
        guard !sorted.isEmpty else { return nil }
        func pct(_ p: Double) -> Double {
            let i = min(Int(Double(sorted.count) * p), sorted.count - 1)
            return Double(sorted[i]) / 1000.0 // us -> ms
        }
        return Stats(
            p50Ms: pct(0.50), p95Ms: pct(0.95), p99Ms: pct(0.99),
            count: sorted.count, skewCorrected: corrected)
    }
}
