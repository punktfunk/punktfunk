// The host's state as the menu bar shows it: the tray model of crates/install/punktfunk-tray
// (status.rs), without its service manager. The host is this process, so it can't be stopped
// or missing; it is starting until the first summary and unavailable once the summary has
// been unreachable past the grace.
import Foundation

enum HostState: Equatable {
    case starting
    case running(LocalSummary)
    case unavailable

    /// How long an unreachable summary reads as starting: a restart is not an outage.
    static let startGrace: TimeInterval = 15

    static func map(_ summary: LocalSummary?, unreachableSince: Date?, now: Date) -> HostState {
        if let summary { return .running(summary) }
        guard let since = unreachableSince, now.timeIntervalSince(since) >= startGrace else { return .starting }
        return .unavailable
    }

    var summary: LocalSummary? {
        if case .running(let s) = self { s } else { nil }
    }

    /// A live session counts even when the video flag is false.
    var isStreaming: Bool { summary.map { $0.videoStreaming || $0.session != nil } ?? false }
    var keptDisplays: Int { summary?.keptDisplays ?? 0 }
    var pairingAttention: Bool { summary.map { $0.pinPending || $0.pendingApprovals > 0 } ?? false }

    /// The status line under the name in the panel.
    var line: String {
        switch self {
        case .starting: String(localized: L10n.trayMacStateStarting)
        case .unavailable: String(localized: L10n.trayMacStateUnavailable)
        case .running(let s):
            if isStreaming, let m = s.session {
                L10n.trayMacStateStreamingMode(width: "\(m.width)", height: "\(m.height)", fps: "\(m.fps)")
            } else if isStreaming {
                String(localized: L10n.trayMacStateStreaming)
            } else if keptDisplays > 0 {
                L10n.trayMacStateIdleKept(count: keptDisplays)
            } else {
                String(localized: L10n.trayMacStateIdle)
            }
        }
    }

    /// The menu bar item's tooltip and accessibility label: the trays' headline.
    var headline: String {
        switch self {
        case .starting: String(localized: L10n.trayStatusStarting)
        case .unavailable: String(localized: L10n.trayStatusDegraded)
        case .running(let s):
            if isStreaming, let m = s.session {
                L10n.trayStatusStreamingMode(version: s.version, width: "\(m.width)", height: "\(m.height)", fps: "\(m.fps)")
            } else if isStreaming {
                L10n.trayStatusStreaming(version: s.version)
            } else if keptDisplays > 0 {
                L10n.trayStatusIdleKept(version: s.version, count: keptDisplays)
            } else {
                L10n.trayStatusIdle(version: s.version)
            }
        }
    }
}

/// The menu bar glyph: filled while streaming, a badge when something waits for the person,
/// struck through when the host does not answer. Shape, never colour, carries the state.
struct GlyphState: Equatable, Hashable, CaseIterable {
    var filled: Bool
    var badge: Bool
    var struck: Bool

    static func of(_ state: HostState, needsAttention: Bool) -> GlyphState {
        if state == .unavailable { return GlyphState(filled: false, badge: false, struck: true) }
        return GlyphState(filled: state.isStreaming, badge: needsAttention, struck: false)
    }

    static let allCases: [GlyphState] = [
        GlyphState(filled: false, badge: false, struck: false),
        GlyphState(filled: true, badge: false, struck: false),
        GlyphState(filled: false, badge: true, struck: false),
        GlyphState(filled: true, badge: true, struck: false),
        GlyphState(filled: false, badge: false, struck: true),
    ]
}

/// The connect notification fires on the idle → streaming edge only, never for a session that
/// was already running when the UI started.
struct ConnectEdge {
    private var seen: Bool?

    mutating func fires(streaming: Bool) -> Bool {
        defer { seen = streaming }
        return seen == false && streaming
    }
}

enum Format {
    /// `m:ss` for a countdown.
    static func clock(_ seconds: Int) -> String {
        let s = max(0, seconds)
        return String(format: "%d:%02d", s / 60, s % 60)
    }

    /// The first 16 hex digits of a fingerprint in groups of four, as the web console shows it.
    static func deviceCode(_ fingerprint: String) -> String {
        let digits = Array(fingerprint.prefix(16).uppercased())
        return stride(from: 0, to: digits.count, by: 4)
            .map { String(digits[$0..<min($0 + 4, digits.count)]) }
            .joined(separator: " ")
    }

    /// `WxH@Hz` from the status rows.
    static func mode(_ text: String) -> (width: Int, height: Int, fps: Int)? {
        let parts = text.split(whereSeparator: { "x@".contains($0) }).compactMap { Int($0) }
        return parts.count == 3 ? (parts[0], parts[1], parts[2]) : nil
    }

    /// The codec's product name; an unknown token passes through.
    static func codec(_ token: String) -> String {
        ["h264": "H.264", "hevc": "HEVC", "av1": "AV1", "pyrowave": "PyroWave"][token] ?? token
    }

    static func bitrate(_ kbps: Int) -> String {
        let mbps = Double(kbps) / 1000
        return L10n.commonMbits(value: mbps.formatted(.number.precision(.fractionLength(mbps < 10 ? 1 : 0))))
    }

    static func age(_ seconds: Int) -> String {
        seconds < 60 ? String(localized: L10n.trayMacPendingAgeNow) : L10n.trayMacPendingAgeMins(count: seconds / 60)
    }
}
