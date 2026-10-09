// What a stream shows the player for a few seconds or for the session — hints, notices and the
// access chip. One value, so ending a session resets all of it in one assignment and a new field
// cannot be forgotten by the teardown.

import Foundation
import PunktfunkKit

/// A notice shown for a while, then cleared.
struct Flash<Value> {
    private(set) var value: Value?
    private var timer: Task<Void, Never>?

    var isShown: Bool { value != nil }

    /// Show `value`; `expire` runs after `seconds` unless a newer show or a clear comes first.
    mutating func show(_ value: Value, for seconds: UInt64, expire: @escaping @MainActor () -> Void) {
        self.value = value
        timer?.cancel()
        timer = Task { @MainActor in
            try? await Task.sleep(for: .seconds(seconds))
            if !Task.isCancelled { expire() }
        }
    }

    mutating func clear() {
        timer?.cancel()
        timer = nil
        value = nil
    }
}

struct SessionNotices {
    /// How long a hint stays up — the start-of-stream shortcut banner's 6 s, since they share
    /// the bottom-centre stack and a player reads them the same way.
    static let hintSeconds: UInt64 = 6
    /// How long a launch line stays up: long enough to read a sentence with its cause.
    static let launchSeconds: UInt64 = 10

    /// A pad whose motion this session cannot carry. The gyro otherwise just does nothing, and
    /// the fix is a settings change, so the hint names it.
    var motionUnreachable = Flash<PunktfunkConnection.GamepadType>()
    /// The "Steam Controller passing through" badge, the SC2 capture's only UI surface. A
    /// release drops it at once: it must never outlive the passthrough it announces.
    var sc2Captured = Flash<Bool>()
    /// How to leave, from stream start, unless the player turned it off.
    var exitHint = Flash<Bool>()
    /// The touch model is passthrough but this host drops contacts, so the trackpad model runs.
    var touchFallback = Flash<Bool>()
    /// "Access ends in 5 m", around the T−5 m / T−1 m marks the host also warns at.
    var accessWarning = Flash<String>()
    /// The host's line for a launch that did not give the player their game.
    var launch = Flash<String>()
    /// The last launch line shown, so one verdict is raised once.
    var launchShown: String?

    /// The session's access preset, derived live from the grants mask. `.fullControl` against
    /// every old host and every full-grant device.
    var accessLevel: PunktfunkConnection.AccessLevel = .fullControl
    /// Seconds until this session's access expires; 0 is permanent. Ticks at the 1 Hz stats
    /// cadence.
    var accessRemainingSecs: UInt32 = 0
    /// Anything about access differs from full-and-permanent: the chip's visibility gate.
    var accessLimited = false
    /// One shot per warning mark; an edit that moves the deadline back above a mark re-arms it.
    var accessWarned5m = false
    var accessWarned1m = false

    /// Stop every pending clear, so none fires into the next session.
    mutating func cancelTimers() {
        motionUnreachable.clear()
        sc2Captured.clear()
        exitHint.clear()
        touchFallback.clear()
        accessWarning.clear()
        launch.clear()
    }
}
