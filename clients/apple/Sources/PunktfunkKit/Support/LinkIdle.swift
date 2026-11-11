// Whether a window is free to start what a link or a Mac host page hands it. `ContentView`
// gathers the facts; `DeepLinkRouter` refuses a link in a busy window, and `LinkArbiter` leaves a
// host page's dial to an idle one. Pure, so `swift test` covers it (`LinkIdleTests`).

import Foundation

public enum LinkIdle {
    /// No session is dialing, at the trust prompt or streaming.
    public static func isIdle(phase: SessionPhase) -> Bool {
        phase == .idle
    }
}
