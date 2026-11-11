// Which library work may still touch the shelf the console has open. The open shelf is the last
// `FetchLibrary`'s host and the serial its list fetch took; the console's service asks here before
// a fetch starts and before each answer goes up. `FetchGuardTests` pins it.

import Foundation

/// The shelf the console has open, as the library service sees it.
public struct FetchGuard: Equatable, Sendable {
    /// What reaches the shelf.
    public enum Arrival: Equatable, Sendable {
        /// A list fetch: `FetchLibrary`, or the refetch after a removal.
        case list
        /// A read of what runs and downloads alone: `RefreshRunning`, or the re-read after End
        /// Game or a download change.
        case refresh
        /// An event of the fetch, or the answer of the refresh, that started under `serial`.
        case answer(serial: Int)
    }

    /// The last `FetchLibrary`'s host fingerprint.
    public var fp: String
    /// Bumped by each list fetch.
    public var serial: Int

    public init(fp: String, serial: Int) {
        self.fp = fp
        self.serial = serial
    }

    /// Whether `arrival` for the host `fp` may touch this shelf. Everything is for its host, so
    /// one host's titles and downloads never go up under another's shelf; an answer is also from
    /// its current fetch, since a newer one owns the shelf by the time a slow host answers.
    public func accepts(_ arrival: Arrival, fp: String) -> Bool {
        switch arrival {
        case .list, .refresh: fp == self.fp
        case .answer(let serial): serial == self.serial && fp == self.fp
        }
    }
}
