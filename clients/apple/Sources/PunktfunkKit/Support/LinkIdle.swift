// Whether a window is free to start what a link hands it. `ContentView` gathers the facts and
// `DeepLinkRouter` refuses a link in a busy window. Pure, so `swift test` covers it
// (`LinkIdleTests`).

import Foundation

public enum LinkIdle {
    /// No session is dialing, at the trust prompt or streaming.
    public static func isIdle(phase: SessionPhase) -> Bool {
        phase == .idle
    }
}
