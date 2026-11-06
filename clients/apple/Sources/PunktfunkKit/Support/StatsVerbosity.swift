// The stats overlay's tier: Off → Compact → Normal → Detailed, the four every client cycles.
// `DefaultsKey.statsVerbosity` stores the tier a session STARTS at; the in-stream cycle (⌃⌥⇧S,
// the three-finger tap, Select + X) moves only the session it was pressed in.
//
// Lives in PunktfunkKit (not the app) because the kit's input paths (TouchMouse's three-finger
// tap, InputCapture's captured-state ⌃⌥⇧S) ask for the cycle directly.

import Foundation
import PunktfunkShared

/// How much of the streaming statistics overlay to show. The raw values are stable on disk —
/// rename the cases freely, never the strings.
public enum StatsVerbosity: String, CaseIterable, Sendable {
    case off, compact, normal, detailed

    /// User-facing tier label (Settings pickers, the gamepad settings row).
    public var label: String {
        switch self {
        case .off: return String(localized: L10n.playerStatsVerbosityOff)
        case .compact: return String(localized: L10n.playerStatsVerbosityCompact)
        case .normal: return String(localized: L10n.playerStatsVerbosityNormal)
        case .detailed: return String(localized: L10n.playerStatsVerbosityDetailed)
        }
    }

    /// The next tier in the cycle: off → compact → normal → detailed → off (wrapping) —
    /// the ⌃⌥⇧S / three-finger-tap order, same as Android.
    public func next() -> StatsVerbosity {
        switch self {
        case .off: return .compact
        case .compact: return .normal
        case .normal: return .detailed
        case .detailed: return .off
        }
    }

    /// The persisted tier. When `statsVerbosity` has never been written, migrates from the
    /// legacy `hudEnabled` bool the pre-tiered clients stored: absent-or-true → `.normal`
    /// (the old "on" look minus the equation lines), explicit false → `.off`.
    public static var current: StatsVerbosity {
        StatsVerbosity(rawValue: EffectiveSettings.storedStatsVerbosity(.standard)) ?? .normal
    }

    /// Persist a tier (the Settings pickers write the same key via @AppStorage).
    public static func store(_ tier: StatsVerbosity) {
        UserDefaults.standard.set(tier.rawValue, forKey: DefaultsKey.statsVerbosity)
    }

    /// Ask `connection`'s session to advance its overlay one tier. nil asks every session, for
    /// the touch and remote paths, which run one session and hold no connection.
    public static func requestCycle(for connection: AnyObject?) {
        // Observers are views; the gamepad surfaces call from their own queues.
        DispatchQueue.main.async {
            NotificationCenter.default.post(name: .punktfunkStatsCycled, object: connection)
        }
    }
}
