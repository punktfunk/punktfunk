// The speed test's goodput over its test traffic, for the page's chart and live number. The
// network check runs several rounds, each reporting cumulative counters from zero. The page polls
// them while the check runs; every poll becomes one point, exponentially smoothed, on a time axis
// that lays the rounds end to end and skips the quiet between them.

import Foundation

extension PunktfunkConnection {
    /// Smoothed goodput per poll of `probeResult()`, for charting the check as it lands.
    public struct ProbeTrace: Sendable, Equatable {
        public struct Point: Sendable, Equatable {
            /// Seconds of test traffic up to this point, earlier rounds included.
            public let seconds: Double
            public let mbps: Double
        }

        /// The smoothing's time constant. Packets arrive in bursts: unsmoothed, a 20 ms poll
        /// draws the bursts. 200 ms settles a step to within 5 % in 0.6 s and leaves no corners.
        static let smoothingMs: Double = 200
        /// The first point waits for this much traffic: less is a few packets' worth.
        static let minSpanMs: UInt32 = 100

        public private(set) var points: [Point] = []
        /// The round's last counted report; a round starts from zero.
        private var lastMs: UInt32 = 0
        private var lastBytes: UInt64 = 0
        /// The smoothed rate, Mbit/s. It carries across rounds, which run at one rate.
        private var smoothed: Double?
        /// Seconds the earlier rounds ran.
        private var before: Double = 0

        public init() {}

        /// The rate to show as a number: the curve's value at its last `step` boundary, so the
        /// digits change a few times a second and not at every poll.
        public func live(every step: Double = 0.2) -> Double? {
            guard let last = points.last else { return nil }
            let boundary = (last.seconds / step).rounded(.down) * step
            return (points.last { $0.seconds <= boundary } ?? points[0]).mbps
        }

        /// Adds one report. One that does not move its round's clock adds nothing.
        public mutating func add(_ result: ProbeResult) {
            // Counters that run backwards are the next round's: each starts again from zero.
            if result.elapsedMs < lastMs || result.recvBytes < lastBytes {
                before += Double(lastMs) / 1000
                lastMs = 0
                lastBytes = 0
            }
            guard result.elapsedMs > lastMs else { return }
            if smoothed == nil, result.elapsedMs < Self.minSpanMs { return }
            let span = Double(result.elapsedMs - lastMs)
            let rate = Double(result.recvBytes - lastBytes) * 8 / span / 1000
            lastMs = result.elapsedMs
            lastBytes = result.recvBytes
            let value = smoothed.map { $0 + (1 - exp(-span / Self.smoothingMs)) * (rate - $0) } ?? rate
            smoothed = value
            points.append(Point(seconds: before + Double(result.elapsedMs) / 1000, mbps: value))
        }
    }
}
