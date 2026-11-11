// Who draws the window: the app's own SwiftUI screens or the console. `ContentView` reads every
// hand-off between the two from one `ScreenOwner`, which `resolve` derives from the session's
// phase, the console's launch hold and a pending error. Pure, so `swift test` covers every
// combination (`ScreenOwnerTests`).

import Foundation

/// Where the app's session stands.
public enum SessionPhase: Equatable {
    case idle
    case connecting
    /// Connected to an unpinned host: the stream is live (and pumping — the opening IDR must not
    /// be missed) but input and cursor capture wait for the user to confirm the fingerprint.
    case awaitingTrust(fingerprint: Data)
    case streaming
}

extension SessionPhase {
    /// The pin a redial after a lost session carries. Only a session the player accepted
    /// (`streaming`) vouches for the host's own fingerprint; one lost at the trust prompt keeps
    /// the pin its dial had, so an unpinned host is never dialed again as trusted.
    public func lossRedialPin(dialed: Data?, live: Data) -> Data? {
        self == .streaming ? live : dialed
    }
}

public struct ScreenOwner: Equatable {
    /// The console draws every prompt: connect, wake, pairing, the approval wait, a failed dial,
    /// its launch hold. The app's alerts, sheets and connect overlay stand down, or they would be
    /// a second interface over it, and on a TV a focus trap the pad cannot reach.
    public var consoleOwnsScreen: Bool
    /// Home is mounted: no stream, or one the console's launch hold still covers.
    public var showsHome: Bool
    /// The console is on screen as home (a phone holds landscape under it).
    public var showsConsole: Bool
    /// The stream takes input: no trust card over it and no console hold.
    public var streamTakesInput: Bool
    /// The "Connection failed" alert may show.
    public var showsError: Bool

    /// `console`: the console is the home screen. `live`: a connection is up. `consoleHold`: the
    /// console reports its launch hold. `error`: an error waits to be shown. The three fullscreen
    /// facts are the Mac window's; every other platform passes false.
    public static func resolve(
        console: Bool, phase: SessionPhase, live: Bool, consoleHold: Bool, error: Bool,
        fullscreen: Bool, appDrivenFullscreen: Bool, fullscreenAlways: Bool
    ) -> ScreenOwner {
        let trustCard = if case .awaitingTrust = phase { true } else { false }
        // The trust card is the stream view's, so the hold gives way to it.
        let holdsStream = consoleHold && !trustCard
        // Once the stream shows, the app owns the screen again, trust card included.
        let owns = console && (phase == .idle || phase == .connecting || holdsStream)
        let showsHome = !live || holdsStream
        // A sheet on a fullscreen window makes AppKit drop `-toggleFullScreen:`, so the alert
        // waits while a fullscreen this app drove leaves. Under Always nothing leaves.
        let leaving = appDrivenFullscreen && fullscreen && !fullscreenAlways
        return ScreenOwner(
            consoleOwnsScreen: owns,
            showsHome: showsHome,
            showsConsole: console && showsHome,
            streamTakesInput: !trustCard && !holdsStream,
            showsError: error && !leaving && !owns)
    }
}
