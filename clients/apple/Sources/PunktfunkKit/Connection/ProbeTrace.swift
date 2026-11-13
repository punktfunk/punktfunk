// The speed test's goodput over its test traffic, for the page's chart. The network check runs
// several rounds, each reporting cumulative counters from zero. The page polls them while the
// check runs; every poll becomes one point: the rate over the last `windowMs` of its round,
// placed on a time axis that lays the rounds end to end and skips the quiet between them.

import Foundation

extension PunktfunkConnection {
    /// Goodput per poll of `probeResult()`, for charting the check's rounds as they land.
    public struct ProbeTrace: Sendable, Equatable {
        public struct Point: Sendable, Equatable {
            /// Seconds of test traffic up to this point, earlier rounds included.
            public let seconds: Double
            public let mbps: Double
        }

        /// What one point averages over. Packets arrive in bursts, so a shorter span draws the
        /// bursts and a longer one hides a real dip: 250 ms is five polls at the page's 50 ms.
        static let windowMs: UInt32 = 250
        /// A round's first point waits for this much of it: less is a few packets' worth.
        static let minSpanMs: UInt32 = 100

        private struct Sample: Sendable, Equatable {
            let ms: UInt32
            let bytes: UInt64
        }

        public private(set) var points: [Point] = []
        /// This round's reports inside the window, oldest first, from its zero.
        private var samples = [Sample(ms: 0, bytes: 0)]
        /// Seconds the earlier rounds ran.
        private var before: Double = 0

        public init() {}

        /// Adds one report. One that does not move its round's clock adds nothing.
        public mutating func add(_ result: ProbeResult) {
            let last = samples[samples.count - 1]
            // Counters that run backwards are the next round's: each starts again from zero.
            if result.elapsedMs < last.ms || result.recvBytes < last.bytes {
                before += Double(last.ms) / 1000
                samples = [Sample(ms: 0, bytes: 0)]
            }
            guard result.elapsedMs > samples[samples.count - 1].ms else { return }
            samples.append(Sample(ms: result.elapsedMs, bytes: result.recvBytes))
            // The newest report a full window back, else the round's start.
            let start = samples.lastIndex { result.elapsedMs - $0.ms >= Self.windowMs } ?? 0
            samples.removeFirst(start)
            let from = samples[0]
            let span = result.elapsedMs - from.ms
            guard span >= Self.minSpanMs else { return }
            let bits = Double(result.recvBytes - from.bytes) * 8
            points.append(Point(
                seconds: before + Double(result.elapsedMs) / 1000,
                mbps: bits / Double(span) / 1000))
        }
    }
}
