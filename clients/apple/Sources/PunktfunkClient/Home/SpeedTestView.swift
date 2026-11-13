// The network speed test as a page (roadmap §9): the host bursts probe filler over the real data
// plane, the page charts the goodput as it lands and recommends ~70% of it as the bitrate. It runs
// only while idle, since the host serves one session at a time. An unpinned host is probed
// trust-on-first-use without persisting anything: a bandwidth number is no trust decision.

import Charts
import Foundation
import PunktfunkKit
import SwiftUI

/// How often the page reads the check's counters while it runs: twenty points a second, each
/// smoothed over `ProbeTrace.windowMs`.
private let pollInterval: Duration = .milliseconds(50)

struct SpeedTestView: View {
    let host: StoredHost
    /// Start on appear, when a tap opened the page to test. A sidebar row waits for Start.
    var startsOnAppear = true
    #if DEBUG
    /// Shot harness: a canned run in place of the network (`ShotGallery.swift`).
    var shotRun: (phase: SpeedTestPhase, trace: PunktfunkConnection.ProbeTrace)?
    #endif

    @AppStorage(DefaultsKey.bitrateKbps) private var bitrateKbps = SettingDefault.bitrateKbps
    /// The catalog, so Apply writes the layer this host reads its bitrate from.
    @ObservedObject private var presets = PresetStore.shared
    /// The phase and the check's report behind `.done` (`SpeedTestRun`).
    @State private var state = SpeedTestRun.State()
    @State private var trace = PunktfunkConnection.ProbeTrace()
    /// What the last Apply changed, said under the buttons.
    @State private var applied: String?

    /// The probe that has not returned, from this page or one closed while it ran. App-wide: the
    /// core call can't be aborted, and a second probe would share the link it measures.
    @MainActor private static var probe: Task<SpeedTestRun.Event, Never>?

    #if os(tvOS)
    private let chartHeight: CGFloat = 360
    #else
    private let pagePadding: CGFloat = 20
    private let chartHeight: CGFloat = 240
    #endif

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                summary
                chart
                    .frame(height: chartHeight)
                if case .done(let result) = phase {
                    stats(result)
                    if let report { findings(report) }
                }
                controls
                Text(L10n.appleSpeedtestAboutBody(host: host.displayName))
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
            }
            #if os(tvOS)
            // The host page's pane, or a pushed page, already keeps it off the screen's edges.
            .frame(maxWidth: .infinity, alignment: .leading)
            #else
            .padding(pagePadding)
            .frame(maxWidth: 760, alignment: .leading)
            .frame(maxWidth: .infinity)
            #endif
        }
        .onAppear {
            #if DEBUG
            if let shotRun {
                state = SpeedTestRun.State(phase: shotRun.phase)
                trace = shotRun.trace
                return
            }
            #endif
            if !state.inFlight, let out = Self.probe {
                follow(out)
            } else if startsOnAppear, phase == .idle {
                run()
            }
        }
    }

    private var phase: SpeedTestPhase { state.phase }
    private var report: PunktfunkConnection.HealthReport? { state.report }

    // MARK: - Pieces

    /// The number the page is about: the measured goodput, or the latest slice while it runs.
    private var summary: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(headline)
                .font(.system(.largeTitle, design: .rounded).weight(.semibold))
                .monospacedDigit()
                .contentTransition(.numericText())
            Text(caption)
                .font(.geist(15, relativeTo: .subheadline))
                .foregroundStyle(isFailed ? Color.red : Color.secondary)
        }
    }

    private var headline: String {
        switch phase {
        case .done(let result): Self.mbpsLabel(kbps: Int(result.throughputKbps))
        // The rounds run below what the link carries, so their live rate is not the answer.
        case .probing: "…"
        case .idle, .connecting, .failed: "—"
        }
    }

    private var caption: String {
        switch phase {
        case .idle: L10n.appleSpeedtestReadyBody(host: host.displayName)
        case .connecting: L10n.appleSpeedtestConnectingHost(host: host.displayName)
        case .probing: String(localized: L10n.speedtestMeasuringBody)
        case .done: Self.doneCaption(report, host: host.displayName)
        case .failed(let message): message
        }
    }

    private var isFailed: Bool {
        if case .failed = phase { return true }
        return false
    }

    /// The check's test traffic as it lands, on a time axis that lays its rounds end to end:
    /// each ends on a whole second, so the grid marks them. Once there is an answer: what the link carries, the recommendation
    /// and this host's current bitrate, drawn across it. The rounds run below what the link
    /// carries on purpose, so the curve sits under that line.
    private var chart: some View {
        Chart {
            ForEach(trace.points, id: \.seconds) { point in
                AreaMark(
                    x: .value(String(localized: L10n.appleSpeedtestChartTimeA11y), point.seconds),
                    y: .value(String(localized: L10n.appleSpeedtestChartGoodputA11y), point.mbps))
                    .foregroundStyle(
                        .linearGradient(
                            colors: [Color.brand.opacity(0.35), Color.brand.opacity(0.02)],
                            startPoint: .top, endPoint: .bottom))
                    .interpolationMethod(.monotone)
                LineMark(
                    x: .value(String(localized: L10n.appleSpeedtestChartTimeA11y), point.seconds),
                    y: .value(String(localized: L10n.appleSpeedtestChartGoodputA11y), point.mbps))
                    .foregroundStyle(Color.brand)
                    .lineStyle(StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round))
                    .interpolationMethod(.monotone)
            }
            if case .done(let result) = phase {
                rule(
                    L10n.speedtestMeasured(rate: Self.mbpsLabel(kbps: Int(result.throughputKbps))),
                    kbps: Int(result.throughputKbps),
                    annotation: { L10n.speedtestMeasured(rate: $0) },
                    color: .secondary, dash: [])
                if let recommended = Self.recommendedKbps(result) {
                    rule(
                        String(localized: L10n.speedtestRecommended), kbps: recommended,
                        annotation: { L10n.speedtestChartRecommended(rate: $0) },
                        color: .green, dash: [6, 4])
                }
            }
            if currentKbps > 0 {
                rule(
                    String(localized: L10n.appleSpeedtestNowA11y), kbps: currentKbps,
                    annotation: { L10n.appleSpeedtestChartNow(rate: $0) },
                    color: .orange, dash: [2, 3])
            }
        }
        .chartXScale(domain: 0...max(2, (trace.points.last?.seconds ?? 0).rounded(.up)))
        .chartXAxis {
            AxisMarks(values: .stride(by: 1)) { value in
                AxisGridLine()
                AxisValueLabel {
                    if let seconds = value.as(Double.self) { Text("\(Int(seconds)) s") }
                }
            }
        }
        .chartYAxisLabel("Mbit/s")
        .overlay { placeholder }
    }

    /// `name` is the series VoiceOver reads; `annotation` words the label over the line from the
    /// rate's text.
    private func rule(
        _ name: String, kbps: Int, annotation: @escaping (String) -> String, color: Color,
        dash: [CGFloat]
    ) -> some ChartContent {
        RuleMark(y: .value(name, Double(kbps) / 1000))
            .foregroundStyle(color)
            .lineStyle(StrokeStyle(lineWidth: 1.5, dash: dash))
            .annotation(position: .top, alignment: .leading) {
                Text(annotation(Self.mbpsLabel(kbps: kbps)))
                    .font(.geist(11, .medium, relativeTo: .caption2))
                    .foregroundStyle(color)
            }
    }

    @ViewBuilder private var placeholder: some View {
        if trace.points.isEmpty {
            switch phase {
            case .connecting, .probing: ProgressView()
            case .idle:
                Text(L10n.appleSpeedtestChartEmptyBody)
                    .font(.geist(13, relativeTo: .footnote))
                    .foregroundStyle(.secondary)
            case .done, .failed: EmptyView()
            }
        }
    }

    private func stats(_ result: PunktfunkConnection.ProbeResult) -> some View {
        HStack(spacing: 12) {
            // The loss and jitter are the clean round's, at a rate the link holds; a host without
            // a ramp only measured a blast, and a blast's loss is not the link's.
            tile(
                String(localized: L10n.speedtestLoss),
                report?.clean.map { String(format: "%.1f %%", $0.lossPct) } ?? "—")
            tile(
                String(localized: L10n.appleSpeedtestJitter),
                report?.clean.map { String(format: "%.1f ms", Double($0.jitterUs) / 1000) } ?? "—")
            tile(
                String(localized: L10n.speedtestRecommended),
                Self.recommendedKbps(result).map { Self.mbpsLabel(kbps: $0) } ?? "—")
        }
    }

    /// What the check found, one line each.
    private func findings(_ r: PunktfunkConnection.HealthReport) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(r.findings.enumerated()), id: \.offset) { _, f in
                Text(Self.findingText(f))
                    .font(.geist(13, relativeTo: .footnote))
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// The caption once there is an answer: what the link carries and, with a clean round, the
    /// rate that loss and jitter were measured at.
    static func doneCaption(_ r: PunktfunkConnection.HealthReport?, host: String) -> String {
        guard let r else { return L10n.appleSpeedtestMeasuredFromBody(host: host) }
        let ceiling = mbpsLabel(kbps: Int(r.ceilingKbps))
        let carries =
            r.wall
            ? L10n.speedtestLinkCarriesBody(rate: ceiling)
            : L10n.speedtestLinkCarriesAtLeastBody(rate: ceiling)
        guard let c = r.clean else { return carries }
        return carries + " " + L10n.appleSpeedtestMeasuredAtBody(rate: mbpsLabel(kbps: Int(c.rateKbps)))
    }

    /// A finding in words — what did not happen, then the next move — the same sentences every
    /// shell shows.
    static func findingText(_ f: PunktfunkConnection.HealthFinding) -> String {
        let a = Int(f.numbers.first ?? 0)
        let b = Int(f.numbers.dropFirst().first ?? 0)
        switch f.id {
        case 1:
            return a > 0 && b > 0
                ? L10n.findingSpeedMismatchBody(hostMbit: String(a), deviceMbit: String(b))
                : String(localized: L10n.findingSpeedMismatchUnknownBody)
        case 2:
            return L10n.findingBurstIntolerantBody(loss: String(format: "%.1f", Double(a) / 100))
        case 3:
            return a > 0
                ? L10n.findingReceiveBufferBody(packets: a, capKb: String(b))
                : L10n.findingReceiveBufferCapBody(capKb: String(b))
        case 4:
            return L10n.findingLinkFaultBody(loss: String(format: "%.1f", Double(a) / 100))
        case 5:
            return L10n.findingQueueBuildUpBody(
                spreadMs: String(format: "%.0f", Double(a) / 1000),
                maxMbit: String(format: "%.0f", Double(b) / 1000))
        case 6:
            return a > 0
                ? L10n.findingHostSendBufferBody(packets: a)
                : L10n.findingHostSendBufferCapBody(capKb: String(b))
        case 7:
            return a > 0
                ? L10n.findingWifiLossBody(loss: String(format: "%.1f", Double(a) / 100))
                : String(localized: L10n.findingWifiBody)
        default:
            return L10n.findingUnknownBody(id: String(f.id))
        }
    }

    private func tile(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label)
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.geist(20, .semibold, relativeTo: .title3))
                .monospacedDigit()
        }
        // Three tiles share a phone's width: shrink a long label rather than break it.
        .lineLimit(1)
        .minimumScaleFactor(0.6)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
        .background(.quaternary.opacity(0.4), in: .rect(cornerRadius: 12))
    }

    @ViewBuilder private var controls: some View {
        HStack(spacing: 12) {
            switch phase {
            case .idle:
                Button(L10n.appleSpeedtestStart, systemImage: "gauge.with.needle") { run() }
                    .glassProminentButtonStyle()
                    .disabled(!state.canStart)
            case .connecting, .probing:
                Button(L10n.appleSpeedtestStop, role: .cancel) { stop() }
            case .done(let result):
                if let recommended = Self.recommendedKbps(result) {
                    applyButtons(recommended)
                }
                Button(L10n.appleSpeedtestRunAgain) { run() }
                    .disabled(!state.canStart)
            case .failed:
                Button(L10n.appleSpeedtestTryAgain) { run() }
                    .glassProminentButtonStyle()
                    .disabled(!state.canStart)
            }
        }
        if let applied {
            Label(applied, systemImage: "checkmark.circle.fill")
                .font(.geist(13, relativeTo: .footnote))
                .foregroundStyle(.green)
        }
    }

    /// Apply writes the layer this host resolves its bitrate from: the global for an unbound host,
    /// the bound preset's override when it has one, and both offered when the preset inherits,
    /// since either is a defensible answer. Every button names its target.
    @ViewBuilder private func applyButtons(_ recommended: Int) -> some View {
        let bound = presets.binding(for: host)
        let label = Self.mbpsLabel(kbps: recommended)
        if let bound {
            Button(L10n.speedtestApplyToPreset(preset: bound.name)) {
                presets.setOverride(bound.id, \.bitrateKbps, recommended)
                applied = L10n.speedtestAppliedPreset(rate: label, preset: bound.name)
            }
            .glassProminentButtonStyle()
            if bound.overrides.bitrateKbps == nil {
                Button(L10n.speedtestSetDefault) {
                    bitrateKbps = recommended
                    applied = L10n.speedtestAppliedDefault(rate: label)
                }
            }
        } else {
            Button(L10n.speedtestUseRate(rate: label)) {
                bitrateKbps = recommended
                applied = L10n.speedtestAppliedDefault(rate: label)
            }
            .glassProminentButtonStyle()
        }
    }

    /// What this host streams at now: its preset's bitrate, else the default. 0 is automatic.
    private var currentKbps: Int {
        EffectiveSettings.resolve(host: host, catalog: presets.catalog).bitrateKbps
    }

    /// ~70% of the measured goodput, whole Mbps, clamped to the host's session bitrate ceiling
    /// (2 Gbps — it clamps any session request above that, so recommending more is pointless).
    /// nil when the measurement carried too little signal to recommend anything.
    static func recommendedKbps(_ result: PunktfunkConnection.ProbeResult) -> Int? {
        guard result.throughputKbps >= 2_000 else { return nil }
        let raw = Int(result.throughputKbps) * 7 / 10
        let wholeMbps = max(raw / 1_000, 2)
        return min(wholeMbps, 2_000) * 1_000
    }

    static func mbpsLabel(kbps: Int) -> String {
        if kbps >= 1_000_000 {
            let gbps = Double(kbps) / 1_000_000
            return L10n.commonGbits(
                value: gbps == gbps.rounded() ? String(Int(gbps)) : String(format: "%.1f", gbps))
        }
        return L10n.commonMbits(
            value: kbps % 1_000 == 0
                ? String(kbps / 1_000) : String(format: "%.1f", Double(kbps) / 1_000))
    }

    // MARK: - The probe

    private func send(_ event: SpeedTestRun.Event) {
        state = SpeedTestRun.reduce(state, event)
    }

    private func stop() {
        send(.stop)
    }

    /// Waits out a probe another page started; its last word frees Start here.
    private func follow(_ out: Task<SpeedTestRun.Event, Never>) {
        send(.busy)
        Task { send(await out.value) }
    }

    /// Starts a probe, unless one is still out (`SpeedTestRun`).
    private func run() {
        if !state.inFlight, let out = Self.probe { follow(out) }
        guard state.canStart else { return }
        send(.start)
        trace = PunktfunkConnection.ProbeTrace()
        applied = nil
        let address = host.address
        let port = host.port
        let pin = host.pinnedSHA256
        // Probe at the mode this host would actually stream at: the measurement IS the streaming
        // path, so it should be the streaming path's mode.
        let (w, h, fps) = EffectiveSettings.resolve(host: host, catalog: presets.catalog)
            .streamMode(native: NativeDisplay.mode)
        let probe = Task.detached(priority: .userInitiated) { () -> SpeedTestRun.Event in
            // Same identity and trust as a session, but a TOFU result is not persisted from here.
            let identity = (try? ClientIdentityStore.shared.load())?.identity
            let conn: PunktfunkConnection
            do {
                // A diagnostic session: probes only.
                conn = try PunktfunkConnection(
                    host: address, port: port, width: w, height: h, refreshHz: fps,
                    pinSHA256: pin, identity: identity,
                    deliveryFlags: PunktfunkConnection.deliveryProbeOnly)
            } catch {
                return .unreachable(L10n.appleSpeedtestUnreachableBody(address: address))
            }
            defer { conn.close() }

            await MainActor.run { send(.connected) }
            // The check answers once, at its end. Its rounds' counters are read meanwhile, so
            // the chart draws the traffic as it lands.
            let poll = Task.detached(priority: .utility) {
                while !Task.isCancelled {
                    if let now = conn.probeResult() {
                        await MainActor.run { if phase == .probing { trace.add(now) } }
                    }
                    try? await Task.sleep(for: pollInterval)
                }
            }
            defer { poll.cancel() }
            // The whole check, reported once: the ceiling the bring-up ramp proved, a clean round
            // at half of it, two shaped legs, both ends' facts, and the findings. It blocks this
            // detached task for ten to twenty seconds.
            return .checked(conn.networkCheck())
        }
        Self.probe = probe
        Task {
            let end = await probe.value
            Self.probe = nil
            send(end)
        }
    }
}
