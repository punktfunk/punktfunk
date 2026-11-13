// The Live Activity's attributes — the ONE type that must be identical in the app (which starts
// and updates the Activity) and the widget extension (which renders it). Hence it lives in the
// dependency-free shared module.
//
// Gated on `os(iOS)`, NOT `canImport(ActivityKit)`: ActivityKit *imports* on macOS but its types
// are `@available(macOS, unavailable)`, so canImport would wrongly admit this on the macOS build.
// Live Activities are iPhone/iPad only (iPadOS reports os(iOS)).
//
// Naming/shape is a runtime contract: an Activity started by one build is decoded by the extension
// of the same build, so keep `ContentState` Codable-stable across releases the way `StoredHost` is.

#if os(iOS)
import ActivityKit
import Foundation

public struct PunktfunkSessionAttributes: ActivityAttributes {
    // Static for the Activity's whole life (set at request time).
    public let hostID: UUID
    public let hostName: String

    public init(hostID: UUID, hostName: String) {
        self.hostID = hostID
        self.hostName = hostName
    }

    public struct ContentState: Codable, Hashable {
        public enum Stage: String, Codable, Hashable {
            case streaming     // foreground, live
            case background    // backgrounded keep-alive (countdown running)
            case ending        // torn down — final state before dismissal
        }

        public var stage: Stage
        /// Session start — drives `Text(timerInterval:)` for a free client-side ticking clock (no
        /// per-second push needed).
        public var startedAt: Date
        /// e.g. "2560×1440 @120 · HEVC · HDR". Updated only when it actually changes.
        public var modeLine: String
        /// While backgrounded: when the keep-alive auto-disconnect fires — drives the countdown.
        public var backgroundDeadline: Date?

        public init(
            stage: Stage, startedAt: Date, modeLine: String, backgroundDeadline: Date? = nil
        ) {
            self.stage = stage
            self.startedAt = startedAt
            self.modeLine = modeLine
            self.backgroundDeadline = backgroundDeadline
        }
    }
}

#endif
