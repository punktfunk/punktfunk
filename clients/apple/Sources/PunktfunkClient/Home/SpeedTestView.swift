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
                }
                controls
                Text("Measures the stream's own path to \(host.displayName) for five seconds. "
                    + "The recommendation leaves about 30% headroom for encoder bursts.")
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
        case .probing: "Measuring. The host is bursting probe data."
        case .done: "Measured goodput from \(host.displayName)."
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
            tile("Loss", String(format: "%.1f %%", result.lossPct))
            tile(
                "Received",
                ByteCountFormatter.string(
                    fromByteCount: Int64(result.recvBytes), countStyle: .binary))
            tile("Recommended", Self.recommendedKbps(result).map { Self.mbpsLabel(kbps: $0) } ?? "—")
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
                conn = try PunktfunkConnection(
                    host: address, port: port, width: w, height: h, refreshHz: fps,
                    pinSHA256: pin, identity: identity)
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

            conn.startSpeedTest(targetKbps: probeTargetKbps, durationMs: probeDurationMs)
            await MainActor.run { if !token.cancelled { phase = .probing } }

            // Poll until the host's end-of-burst report lands, or a generous deadline: the host
            // clamps the burst to ≤ 5 s.
            let deadline = Date().addingTimeInterval(Double(probeDurationMs) / 1000 + 8)
            var final: PunktfunkConnection.ProbeResult?
            while !token.cancelled, Date() < deadline {
                try? await Task.sleep(nanoseconds: 200_000_000)
                guard let result = conn.probeResult() else { break } // closed underneath us
                await MainActor.run {
                    if !token.cancelled {
                        withAnimation(.easeOut(duration: 0.2)) { trace.add(result) }
                    }
                }
                if result.done {
                    final = result
                    break
                }
            }
            let result = final
            await MainActor.run {
                guard !token.cancelled else { return }
                phase = result.map { .done($0) }
                    ?? .failed("The measurement never finished. The connection may have dropped.")
            }
        }
    }
}
