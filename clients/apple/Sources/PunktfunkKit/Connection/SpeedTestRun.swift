// The speed test page's phase rule: what Start, Stop and the probe's progress do to what the page
// shows. The probe is one blocking core call nothing aborts, so Stop only takes the page back to
// its start and Start waits until the probe returns: a second probe never overlaps the first.
// `SpeedTestView` sends every event through `SpeedTestRun.reduce`. Pure, so `swift test` runs
// every row (`SpeedTestRunTests`).

import PunktfunkShared

/// Where a measurement is.
public enum SpeedTestPhase: Equatable, Sendable {
    case idle
    case connecting
    case probing
    case done(PunktfunkConnection.ProbeResult)
    case failed(String)
}

public enum SpeedTestRun {
    /// What the page shows: the phase, and the check's report behind `.done`.
    public struct State: Equatable, Sendable {
        public var phase: SpeedTestPhase
        public var report: PunktfunkConnection.HealthReport?
        /// A probe has not returned yet, this page's or another's.
        public var inFlight: Bool

        public init(phase: SpeedTestPhase = .idle, inFlight: Bool = false) {
            self.phase = phase
            self.inFlight = inFlight
        }

        /// Start, Run again and Try again wait for the probe that is out.
        public var canStart: Bool { !inFlight }
    }

    public enum Event: Equatable, Sendable {
        /// Start, Run again, Try again, or a page opened to test.
        case start
        case stop
        /// A probe another page started is still out.
        case busy
        /// The probe's session is up.
        case connected
        /// The probe never connected: the sentence to show.
        case unreachable(String)
        /// The probe returned the check's report, or nil when it measured nothing.
        case checked(PunktfunkConnection.HealthReport?)
    }

    /// `state` after `event`. A Start while a probe is out does nothing. A probe's last word
    /// frees Start, and lands only while the page measures: after a Stop it is dropped.
    public static func reduce(_ state: State, _ event: Event) -> State {
        var next = state
        let measuring = state.phase == .connecting || state.phase == .probing
        switch event {
        case .start:
            if state.canStart { next = State(phase: .connecting, inFlight: true) }
        case .stop:
            next.phase = .idle
        case .busy:
            next.inFlight = true
        case .connected:
            if state.phase == .connecting { next.phase = .probing }
        case .unreachable(let why):
            next.inFlight = false
            if measuring { next.phase = .failed(why) }
        case .checked(let report):
            next.inFlight = false
            guard measuring else { break }
            next.report = report
            next.phase = report.map { .done(probeResult(from: $0)) }
                ?? .failed(String(localized: L10n.speedtestNoResultBody))
        }
        return next
    }

    /// The check's report as the page's measurement: the ceiling, and the clean round's loss.
    public static func probeResult(
        from r: PunktfunkConnection.HealthReport
    ) -> PunktfunkConnection.ProbeResult {
        PunktfunkConnection.ProbeResult(
            done: true, recvBytes: 0, recvPackets: 0, hostBytes: 0, hostPackets: 0, elapsedMs: 0,
            throughputKbps: r.ceilingKbps, lossPct: r.clean?.lossPct ?? 0)
    }
}
