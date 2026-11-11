// The speed test page's phase rule: what Start, Stop and the probe's progress do to what the page
// shows. `SpeedTestView` sends every event through `SpeedTestRun.reduce`. Pure, so `swift test`
// runs every row (`SpeedTestRunTests`).

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

        public init(phase: SpeedTestPhase = .idle) {
            self.phase = phase
        }
    }

    public enum Event: Equatable, Sendable {
        /// Start, Run again, Try again, or a page opened to test.
        case start
        case stop
        /// The probe's session is up.
        case connected
        /// The probe never connected: the sentence to show.
        case unreachable(String)
        /// The probe returned the check's report, or nil when it measured nothing.
        case checked(PunktfunkConnection.HealthReport?)
    }

    /// `state` after `event`. A probe's word lands only while the page measures: after a Stop
    /// it is dropped.
    public static func reduce(_ state: State, _ event: Event) -> State {
        var next = state
        let measuring = state.phase == .connecting || state.phase == .probing
        switch event {
        case .start:
            next = State(phase: .connecting)
        case .stop:
            next.phase = .idle
        case .connected:
            if state.phase == .connecting { next.phase = .probing }
        case .unreachable(let why):
            if measuring { next.phase = .failed(why) }
        case .checked(let report):
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
