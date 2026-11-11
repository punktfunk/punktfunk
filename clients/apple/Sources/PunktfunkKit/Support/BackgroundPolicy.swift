// What backgrounding the app does to the session. `ContentView` reads the player's keep-alive
// switch and the session's phase when the scene goes to the background (or a dial lands there)
// and performs what `decide` returns. Pure, so `swift test` covers it (`BackgroundPolicyTests`).

import Foundation

public enum BackgroundPolicy: Equatable {
    /// Keep audio and the connection, drop video, and end the session after this many minutes.
    case hold(minutes: Int)
    /// End the session, not deliberately: the host keeps the display for a fast reconnect.
    case end
    /// Nothing is streaming yet; a dial that lands in the background is decided then.
    case none

    /// A streaming session is held under the opt-in keep-alive, or ended. A running app keeps
    /// answering the host's keep-alives, so a session left alone in the background never ends.
    public static func decide(phase: SessionPhase, keepAlive: Bool, minutes: Int) -> BackgroundPolicy {
        guard phase == .streaming else { return .none }
        return keepAlive ? .hold(minutes: minutes) : .end
    }
}
