// The per-tick access rules: the T−5 m and T−1 m expiry warnings, and the capture an access
// edit takes away. `SessionModel.updateAccessState` reads the live grants and countdown each
// second and performs what these return. Pure, so `swift test` walks every mark
// (`AccessWarningTests`).

import Foundation

/// The two warnings' latches, one shot each.
public struct AccessWarning: Equatable {
    public var fiveMinutes: Bool
    public var oneMinute: Bool

    public init(fiveMinutes: Bool = false, oneMinute: Bool = false) {
        self.fiveMinutes = fiveMinutes
        self.oneMinute = oneMinute
    }

    /// The toast a crossed mark raises.
    public enum Toast: Equatable {
        /// "Access ends in 4 m", from the seconds left.
        case endsIn(UInt32)
        /// "Access ends soon".
        case endsSoon
    }

    /// The latches at `remaining` seconds (0 is permanent) and the toast a mark crossed raises.
    /// Read off the countdown, not the host's warning messages, which re-anchor the same
    /// countdown: a warning that never lands still fires. An edit that moves the deadline back
    /// above a mark re-arms it.
    public static func step(
        remaining: UInt32, warned: AccessWarning
    ) -> (warned: AccessWarning, toast: Toast?) {
        guard remaining != 0 else { return (warned, nil) }
        if remaining > 300 {
            // The 1 m latch never stands without the 5 m one.
            return (warned.fiveMinutes ? AccessWarning() : warned, nil)
        }
        if remaining > 60 {
            return (AccessWarning(fiveMinutes: true), warned.fiveMinutes ? nil : .endsIn(remaining))
        }
        if warned.oneMinute { return (warned, nil) }
        return (AccessWarning(fiveMinutes: true, oneMinute: true), .endsSoon)
    }

    /// An engaged capture lets go once the grants carry neither pointer nor keyboard: a frozen
    /// cursor and swallowed keys over input the host drops tell the player nothing.
    public static func releasesCapture(captured: Bool, grants: UInt32) -> Bool {
        captured
            && grants & (PunktfunkConnection.grantPointer | PunktfunkConnection.grantKeyboard) == 0
    }
}
