// When a launch hold lifts. `SessionModel.watchLaunch` reads the host's status once a second,
// takes the held title's facts from it and performs the step this returns. Pure, so `swift test`
// covers every state the host reports (`LaunchHoldRuleTests`).

import Foundation

public enum LaunchHoldRule: Equatable {
    /// Hold on, saying whether the host waits for the game's window, or showing its files coming.
    case keep(windowWait: Bool, download: HostDownload?)
    /// Drop the hold: the game is up, or never will be.
    case reveal

    /// The held title's download, as a launch reads it.
    public enum Download: Equatable {
        /// Coming in: the launch waits on it.
        case live(HostDownload)
        /// Failed, cancelled or paused: nothing will start.
        case stopped
    }

    /// What one status answer says about the held title.
    public struct Title: Equatable {
        /// The host's state for it; nil while it lists no lease.
        public var state: String?
        public var awaitingWindow: Bool
        public var download: Download?

        public init(state: String?, awaitingWindow: Bool = false, download: Download? = nil) {
            self.state = state
            self.awaitingWindow = awaitingWindow
            self.download = download
        }

        /// `entry`'s row and download in a status answer.
        public init(of entry: GameEntry, games: [RunningGame], downloads: [HostDownload]) {
            let game = games.first { $0.appID == entry.id }
            state = game?.state
            awaitingWindow = game?.awaitingWindow == true
            download = downloads.first { $0.appID == entry.id }.flatMap {
                $0.live ? .live($0) : $0.stopped(title: entry.title) != nil ? .stopped : nil
            }
        }

        /// The game runs and the host waits for its window.
        var windowWait: Bool { state == "running" && awaitingWindow }
    }

    /// The caps count from now again: the title's files are still coming.
    public var restartsClock: Bool {
        if case .keep(_, .some) = self { return true }
        return false
    }

    /// `title` is nil when the read missed. `elapsed` counts from the hold, or from the end of its
    /// download. `cap` bounds a title the host still starts or whose window it awaits; `noLease`
    /// bounds one it never lists.
    public static func step(
        _ title: Title?, elapsed: TimeInterval, cap: TimeInterval, noLease: TimeInterval
    ) -> LaunchHoldRule {
        // A miss reads as an answer that lists nothing.
        let title = title ?? Title(state: nil)
        // No cap while the files come.
        if case .live(let download) = title.download {
            return .keep(windowWait: title.windowWait, download: download)
        }
        if title.download == .stopped, title.state == nil || title.state == "launching" {
            return .reveal
        }
        let done: Bool
        switch title.state {
        case "launching": done = elapsed >= cap
        // A Proton prefix or a splash can sit behind a running process for a minute.
        case "running" where title.windowWait: done = elapsed >= cap
        // window, running, exited, untracked, grace, detached: the host has said all it will.
        case .some: done = true
        case nil: done = elapsed >= noLease
        }
        return done ? .reveal : .keep(windowWait: title.windowWait, download: nil)
    }
}
