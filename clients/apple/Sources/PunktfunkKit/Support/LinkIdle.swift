// Whether a window is free to start what a link or a Mac host page hands it. `ContentView`
// gathers the facts; `DeepLinkRouter` refuses a link in a busy window, and `LinkArbiter` leaves a
// host page's dial to an idle one. Pure, so `swift test` covers it (`LinkIdleTests`).

import Foundation

public enum LinkIdle {
    /// No session is dialing, at the trust prompt or streaming, and nothing is on its way to a
    /// dial: a lost session's redial, a wake wait that connects after, or a tap's profile fetch.
    /// A link started over one of those would run a second flow beside it. A wake alone is not
    /// `waking`: it ends in no session, and a link's connect to that host must still run.
    public static func isIdle(
        phase: SessionPhase, redialPending: Bool, waking: Bool, preDialFetch: Bool
    ) -> Bool {
        phase == .idle && !redialPending && !waking && !preDialFetch
    }
}
