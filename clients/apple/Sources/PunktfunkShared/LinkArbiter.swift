// Which window acts on a hand-off: a `punktfunk://` link an intent posts to every window, or a
// Mac host page's request. `ContentView` asks in each window and performs the answer; the first
// window to take the hand-off from its inbox runs it. The platform is a value, so the Mac runs
// every row (`LinkArbiterTests`).

import Foundation

public enum LinkArbiter {
    /// How a platform shows this app's windows.
    public enum Platform: Equatable, Sendable {
        /// macOS: windows, one of them key.
        case mac
        /// iPadOS and visionOS: scenes, each foreground-active while in front. An iPhone has one.
        case scenes
        /// tvOS: one window.
        case tv

        public static var current: Platform {
            #if os(macOS)
            .mac
            #elseif os(tvOS)
            .tv
            #else
            .scenes
            #endif
        }
    }

    /// What a window is handed.
    public enum Kind: Equatable, Sendable {
        /// A link, which every window on the platform hears.
        case link(Platform)
        /// A Mac host page's connect or pairing: each opens a connection, so it needs an idle
        /// window.
        case hostDial
        /// A Mac host page's browse or wake.
        case hostOther
    }

    public enum Decision: Equatable, Sendable {
        /// Take it now.
        case runHere
        /// Take it after this many seconds, unless another window has.
        case retryAfter(TimeInterval)
        /// Leave it to another window.
        case leave
    }

    /// Time for the window in front, or an idle one, to take a hand-off first.
    public static let delay: TimeInterval = 0.25

    /// `isBusy`: the window has something under way (`LinkIdle`). `isFront`: it is the key window
    /// on a Mac, a foreground-active scene elsewhere. A busy window still takes a link, and
    /// `DeepLinkRouter` answers it there.
    public static func decide(kind: Kind, isBusy: Bool, isFront: Bool) -> Decision {
        switch kind {
        case .link(.mac): isFront ? .runHere : .retryAfter(delay)
        case .link: .runHere
        case .hostDial: isBusy ? .leave : .runHere
        case .hostOther: .runHere
        }
    }
}
