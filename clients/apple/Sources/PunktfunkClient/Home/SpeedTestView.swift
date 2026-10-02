// The network speed test as a page (roadmap §9): the host bursts probe filler over the real data
// plane, the page charts the goodput as it lands and recommends ~70% of it as the bitrate. It runs
// only while idle, since the host serves one session at a time. An unpinned host is probed
// trust-on-first-use without persisting anything: a bandwidth number is no trust decision.

import Charts
import Foundation
import PunktfunkKit
import SwiftUI

/// Leaving the page abandons the probe: the detached connect/poll loop checks this flag and closes
/// the connection itself. A single Bool, so a torn read between the loop and the main actor is
/// harmless.
private final class ProbeToken: @unchecked Sendable {
    var cancelled = false
}

/// What the host is asked to burst: far more than any link carries (it clamps to ≤ 10 Gbit/s), so the
/// measurement finds where delivery falls off rather than an artificial cap. Five seconds lets the
/// host's send and this device's receive settle; a short probe swings wildly on the same link.
private let probeTargetKbps: UInt32 = 3_000_000
private let probeDurationMs: UInt32 = 5_000

/// Where a measurement is.
enum SpeedTestPhase: Equatable {
    case idle
    case connecting
    case probing
    case done(PunktfunkConnection.ProbeResult)
    case failed(String)
}

struct SpeedTestView: View {
    let host: StoredHost
    /// Start on appear, when a tap opened the page to test. A sidebar row waits for Start.
    var startsOnAppear = true
    #if DEBUG
    /// Shot harness: a canned run in place of the network (`ShotGallery.swift`).
    var shotRun: (phase: SpeedTestPhase, trace: PunktfunkConnection.ProbeTrace)?
    #endif

    @AppStorage(DefaultsKey.bitrateKbps) private var bitrateKbps = 0
    /// The catalog, so Apply writes the layer this host reads its bitrate from.
    @ObservedObject private var presets = PresetStore.shared
    @State private var phase: SpeedTestPhase = .idle
    @State private var trace = PunktfunkConnection.ProbeTrace()
    @State private var token = ProbeToken()
    /// What the last Apply changed, said under the buttons.
    @State private var applied: String?
    /// The check's report behind `phase == .done`: the findings and the offered profile.
    @State private var report: PunktfunkConnection.HealthReport?
    /// Where an offered profile is remembered, on this host's record.
    @ObservedObject private var hostStore = HostStore.shared

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
                Text("Measures the stream's own path to \(host.displayName): what it carries, loss "
                    + "and jitter at a rate it holds, and what the link does. The recommendation "
                    + "leaves about 30% headroom for encoder bursts.")
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
                phase = shotRun.phase
                trace = shotRun.trace
                return
            }
            #endif
            if startsOnAppear, phase == .idle { run() }
        }
        .onDisappear { token.cancelled = true }
    }

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
        case .probing:
            trace.points.last.map { Self.mbpsLabel(kbps: Int($0.mbps.rounded()) * 1_000) } ?? "…"
        case .idle, .connecting, .failed: "—"
        }
    }

    private var caption: String {
        switch phase {
        case .idle: "Ready to measure the link to \(host.displayName)."
        case .connecting: "Connecting to \(host.displayName)…"
        case .probing: "Measuring the link. This takes a few seconds."
        case .done: Self.doneCaption(report, host: host.displayName)
        case .failed(let message): message
        }
    }

    private var isFailed: Bool {
        if case .failed = phase { return true }
        return false
    }

    /// Goodput per poll as it lands, with the average, the recommendation and this host's
    /// current bitrate drawn across it once there is something to compare.
    private var chart: some View {
        Chart {
            ForEach(trace.points, id: \.seconds) { point in
                AreaMark(x: .value("Time", point.seconds), y: .value("Goodput", point.mbps))
                    .foregroundStyle(
                        .linearGradient(
                            colors: [Color.brand.opacity(0.35), Color.brand.opacity(0.02)],
                            startPoint: .top, endPoint: .bottom))
                    .interpolationMethod(.monotone)
                LineMark(x: .value("Time", point.seconds), y: .value("Goodput", point.mbps))
                    .foregroundStyle(Color.brand)
                    .lineStyle(StrokeStyle(lineWidth: 2))
                    .interpolationMethod(.monotone)
            }
            if case .done(let result) = phase {
                rule("Average", kbps: Int(result.throughputKbps), color: .secondary, dash: [])
                if let recommended = Self.recommendedKbps(result) {
                    rule("Recommended", kbps: recommended, color: .green, dash: [6, 4])
                }
            }
            if currentKbps > 0 {
                rule("Now", kbps: currentKbps, color: .orange, dash: [2, 3])
            }
        }
        .chartXScale(domain: 0...Double(probeDurationMs) / 1000)
        .chartXAxis {
            AxisMarks(values: .stride(by: 1)) { value in
                AxisGridLine()
                AxisValueLabel {
                    if let seconds = value.as(Double.self) { Text("\(Int(seconds)) s") }
                }
            }
        }
        .chartYAxisLabel("Mbps")
        .overlay { placeholder }
    }

    private func rule(_ name: String, kbps: Int, color: Color, dash: [CGFloat]) -> some ChartContent {
        RuleMark(y: .value(name, Double(kbps) / 1000))
            .foregroundStyle(color)
            .lineStyle(StrokeStyle(lineWidth: 1.5, dash: dash))
            .annotation(position: .top, alignment: .leading) {
                Text("\(name) \(Self.mbpsLabel(kbps: kbps))")
                    .font(.geist(11, .medium, relativeTo: .caption2))
                    .foregroundStyle(color)
            }
    }

    @ViewBuilder private var placeholder: some View {
        if trace.points.isEmpty {
            switch phase {
            case .connecting, .probing: ProgressView()
            case .idle:
                Text("The chart fills in as the probe runs.")
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
            tile("Loss", report?.clean.map { String(format: "%.1f %%", $0.lossPct) } ?? "—")
            tile(
                "Jitter",
                report?.clean.map { String(format: "%.1f ms", Double($0.jitterUs) / 1000) } ?? "—")
            tile("Recommended", Self.recommendedKbps(result).map { Self.mbpsLabel(kbps: $0) } ?? "—")
        }
    }

    /// What the check found, one line each, and the offer when a finding names a profile.
    private func findings(_ r: PunktfunkConnection.HealthReport) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(r.findings.enumerated()), id: \.offset) { _, f in
                Text(Self.findingText(f))
                    .font(.geist(13, relativeTo: .footnote))
                    .foregroundStyle(.secondary)
            }
            if let profile = r.offeredProfile {
                Button("Use paced delivery (\(Self.profileName(profile)))") {
                    var updated = host
                    updated.delivery = Int(profile)
                    hostStore.update(updated)
                    applied = "Paced delivery is on for \(host.displayName) from the next session."
                }
            }
        }
    }

    /// The caption once there is an answer: what the link carries and, with a clean round, the
    /// rate that loss and jitter were measured at.
    static func doneCaption(_ r: PunktfunkConnection.HealthReport?, host: String) -> String {
        guard let r else { return "Measured goodput from \(host)." }
        let carries = r.wall
            ? "The link to \(host) carries \(mbpsLabel(kbps: Int(r.ceilingKbps)))."
            : "The link to \(host) carries at least \(mbpsLabel(kbps: Int(r.ceilingKbps)))."
        guard let c = r.clean else { return carries }
        return carries + " Measured at \(mbpsLabel(kbps: Int(c.rateKbps)))."
    }

    /// A finding in words — what did not happen, then the next move — the same sentences every
    /// shell shows. The offered profile is the button, not a sentence here.
    static func findingText(_ f: PunktfunkConnection.HealthFinding) -> String {
        let a = Int(f.numbers.first ?? 0)
        let b = Int(f.numbers.dropFirst().first ?? 0)
        switch f.id {
        case 1:
            return a > 0 && b > 0
                ? "The host's port is faster than this device's (\(a) vs \(b) Mbit/s), so bursts "
                    + "overflow the switch between them."
                : "The host's port is faster than this device's, so bursts overflow the switch "
                    + "between them."
        case 2:
            return String(
                format: "This device drops the start of every burst (%.1f %% lost) — the adapter's "
                    + "power saving is the usual cause.", Double(a) / 100)
        case 3:
            return a > 0
                ? "This device's own receive buffer dropped \(a) packets; the system caps it at \(b) KB."
                : "The system caps this device's receive buffer at \(b) KB."
        case 4:
            return String(
                format: "Loss at a rate no link refuses (%.1f %%): check the cable, the port or the "
                    + "adapter driver.", Double(a) / 100)
        case 5:
            return String(
                format: "Something on the path buffers instead of dropping (%.0f ms spread); keep "
                    + "the bitrate under %.0f Mbit/s.", Double(a) / 1000, Double(b) / 1000)
        case 6:
            return a > 0
                ? "The host's send buffer refused \(a) packets; raise its limit."
                : "The host's send buffer is capped at \(b) KB; raise its limit."
        case 7:
            return a > 0
                ? String(format: "This device is on Wi-Fi; bursts lose %.1f %%.", Double(a) / 100)
                : "This device is on Wi-Fi."
        default:
            return "Finding \(f.id)."
        }
    }

    static func profileName(_ profile: UInt8) -> String {
        switch profile {
        case 1: "capped"
        case 2: "smooth"
        default: "none"
        }
    }

    /// The check's report as the page's measurement: the ceiling, and the clean round's loss.
    static func probeResult(from r: PunktfunkConnection.HealthReport) -> PunktfunkConnection.ProbeResult {
        PunktfunkConnection.ProbeResult(
            done: true, recvBytes: 0, recvPackets: 0, hostBytes: 0, hostPackets: 0, elapsedMs: 0,
            throughputKbps: r.ceilingKbps, lossPct: r.clean?.lossPct ?? 0)
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
                Button("Start Test", systemImage: "gauge.with.needle") { run() }
                    .glassProminentButtonStyle()
            case .connecting, .probing:
                Button("Stop", role: .cancel) { stop() }
            case .done(let result):
                if let recommended = Self.recommendedKbps(result) {
                    applyButtons(recommended)
                }
                Button("Run Again") { run() }
            case .failed:
                Button("Try Again") { run() }
                    .glassProminentButtonStyle()
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
            Button("Apply to “\(bound.name)”") {
                presets.setOverride(bound.id, \.bitrateKbps, recommended)
                applied = "“\(bound.name)” streams at \(label) from the next session."
            }
            .glassProminentButtonStyle()
            if bound.overrides.bitrateKbps == nil {
                Button("Set as default (\(label))") {
                    bitrateKbps = recommended
                    applied = "The default bitrate is \(label) from the next session."
                }
            }
        } else {
            Button("Use \(label)") {
                bitrateKbps = recommended
                applied = "Streams use \(label) from the next session."
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
            return gbps == gbps.rounded()
                ? "\(Int(gbps)) Gbps"
                : String(format: "%.1f Gbps", gbps)
        }
        return kbps % 1_000 == 0
            ? "\(kbps / 1_000) Mbps"
            : String(format: "%.1f Mbps", Double(kbps) / 1_000)
    }

    // MARK: - The probe

    private func stop() {
        token.cancelled = true
        phase = .idle
    }

    private func run() {
        // A fresh token per attempt, so abandoning one attempt cannot silence the next.
        token.cancelled = true
        token = ProbeToken()
        let token = token
        phase = .connecting
        trace = PunktfunkConnection.ProbeTrace()
        applied = nil
        report = nil
        let address = host.address
        let port = host.port
        let pin = host.pinnedSHA256
        // Probe at the mode this host would actually stream at: the measurement IS the streaming
        // path, so it should be the streaming path's mode.
        let (w, h, fps) = EffectiveSettings.resolve(host: host, catalog: presets.catalog)
            .streamMode(native: NativeDisplay.mode)
        Task.detached(priority: .userInitiated) {
            // Same identity and trust as a session, but a TOFU result is not persisted from here.
            let identity = (try? ClientIdentityStore.shared.load())?.identity
            let conn: PunktfunkConnection
            do {
                // A diagnostic session: probes only, the host's facts asked for.
                conn = try PunktfunkConnection(
                    host: address, port: port, width: w, height: h, refreshHz: fps,
                    pinSHA256: pin, identity: identity,
                    deliveryFlags: PunktfunkConnection.deliveryFacts
                        | PunktfunkConnection.deliveryProbeOnly)
            } catch {
                await MainActor.run {
                    guard !token.cancelled else { return }
                    phase = .failed(
                        "Couldn't reach \(address). It may be asleep, or streaming to something "
                            + "else.")
                }
                return
            }
            defer { conn.close() }

            await MainActor.run { if !token.cancelled { phase = .probing } }
            // The whole check, reported once: the ceiling the bring-up ramp proved, a clean round
            // at half of it, two shaped legs, both ends' facts, and the findings. It blocks this
            // detached task for ten to twenty seconds.
            let checked = conn.networkCheck()
            await MainActor.run {
                guard !token.cancelled else { return }
                if let r = checked {
                    report = r
                    phase = .done(Self.probeResult(from: r))
                } else {
                    phase = .failed("The measurement never finished. The connection may have dropped.")
                }
            }
        }
    }
}
