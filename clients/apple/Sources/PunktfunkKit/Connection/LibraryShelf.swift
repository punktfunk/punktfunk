// The touch shelf's decisions: which titles each of its rows holds, and what a load event does
// to the shelf. Pure values over `LibraryCollation`'s rules, so `LibraryShelfTests` pins them;
// `LibraryView` gathers the facts and draws the result.

import Foundation
import PunktfunkShared

public enum LibraryShelf {
    /// One headed block of posters.
    public typealias Section = (label: String, games: [GameEntry])

    /// The Library tab's title rows, from one pass over the catalog.
    public struct Rows {
        /// Played titles, newest first, at most `recentLimit`.
        public var recent: [GameEntry]
        /// Favorite titles, in the shelf's sort.
        public var favorites: [GameEntry]
        /// The launcher entries, in the host's order.
        public var launchers: [GameEntry]
        /// The Games section: the titles less their launchers, which have their own row.
        public var games: [Section]
    }

    /// The Recent row's length.
    static let recentLimit = 12

    /// `games` collated by the shared rules: launchers lead, then one section per group under
    /// `groupBy` (none: one section of games), each in `sort`. A running title leads its group
    /// (`LibraryOrder`); the desktop tile is never a section's.
    public static func grid(
        _ games: [GameEntry], sort: LibrarySortKey, groupBy: LibraryGroupBy?,
        running: Set<String>
    ) -> [Section] {
        let ordered = LibraryOrder.display(games, running: running)
        return LibraryCollation.collate(ordered, sort: sort, groupBy: groupBy).map { group in
            // The ungrouped bucket names itself "All"; on this grid it is "Games".
            let label = groupBy == nil && group.key != .launchers
                ? String(localized: L10n.libSectionGames) : group.label
            return (label, group.indices.map { ordered[$0] })
        }
    }

    /// The tab's rows over the titles `search` leaves. Each row is filtered before it is
    /// sorted, so a sort runs over its row and not the shelf.
    public static func rows(
        _ games: [GameEntry], sort: LibrarySortKey, groupBy: LibraryGroupBy?,
        running: Set<String>, favorites: Set<String>, search: String
    ) -> Rows {
        let shelf = search.isEmpty ? games : games.filter { $0.title.localizedStandardContains(search) }
        let titles = shelf.filter { !$0.isLauncher }
        let played = titles.filter { ($0.stats?.lastPlayedUnixMs ?? 0) > 0 }
        let recent = LibraryCollation.collate(played, sort: .recent, groupBy: nil).first?.indices ?? []
        let marked = shelf.filter { favorites.contains($0.id) }
        return Rows(
            recent: recent.prefix(recentLimit).map { played[$0] },
            favorites: LibraryCollation.collate(marked, sort: sort, groupBy: nil)
                .flatMap(\.indices).map { marked[$0] },
            launchers: shelf.filter(\.isLauncher),
            games: grid(titles, sort: sort, groupBy: groupBy, running: running))
    }

    /// The shelf a load fills: its titles, where they came from, and what the host runs.
    public struct State: Equatable {
        public var games: [GameEntry] = []
        public var loading = false
        public var errorText: String?
        /// The host didn't answer and auto-wake is off: the shelf offers a wake.
        public var wakeOffered = false
        /// When the titles on screen were fetched, while they come from disk, not the host.
        public var servedFromCacheAt: Date?
        /// What the host has up, by library id: the Resume badge and End Game.
        public var running: [String: RunningGame] = [:]
        /// `/status` downloads by library id, and this device's grants there.
        public var downloads: [String: HostDownload] = [:]
        public var grants: UInt32?

        public init() {}

        /// Whether the titles are remembered rather than observed, and what the host is doing
        /// about it.
        public var staleness: LibraryStaleness {
            guard servedFromCacheAt != nil else { return .none }
            return loading ? .waking : .offline
        }

        /// A download runs, so the shelf reads `/status` again to move its percentage.
        public var polls: Bool { downloads.values.contains(where: \.live) }

        /// The shelf after a `/status` read; a miss (nil) reads as nothing running.
        public func applying(
            _ answer: (games: [RunningGame], downloads: [HostDownload], grants: UInt32?)?
        ) -> State {
            let answer = answer ?? ([], [], nil)
            var next = self
            next.running = Dictionary(
                answer.games.filter(\.isUp).compactMap { g in g.appID.map { ($0, g) } },
                // Two sessions can have one title up; the endable one carries End Game.
                uniquingKeysWith: { first, other in other.endable == true ? other : first })
            next.downloads = Dictionary(
                answer.downloads.map { ($0.appID, $0) }, uniquingKeysWith: { _, last in last })
            next.grants = answer.grants
            return next
        }
    }

    /// One load event applied to `state`. `autoWake` is the player's switch; `canWake`, whether
    /// a wake can reach the host (`WakePolicy.canWake`).
    public static func reduce(
        _ state: State, _ event: LibraryLoadEvent, autoWake: Bool, canWake: Bool
    ) -> State {
        var next = state
        switch event {
        case .cached(let cached):
            next.games = cached.games
            next.servedFromCacheAt = cached.fetchedAt
        case .waking:
            break
        case .fetched(let games):
            next.games = games
            next.servedFromCacheAt = nil
            next.errorText = nil
        case .failed(let error, _):
            // Titles on screen outrank the error; the staleness note carries the situation.
            if next.games.isEmpty { next.errorText = LibraryLoad.failure(error) }
            if case .unreachable = error, !autoWake, canWake { next.wakeOffered = true }
        case .status(let games, let downloads, let grants):
            next = next.applying((games, downloads, grants))
            next.loading = false
        }
        return next
    }
}

/// The catalog's provenance, as the shelf states it. Three states rather than a flag, so
/// "waking the host…" never shows while nothing happens: the desktop console's
/// `Stale::{No, Waking, Offline}`, with its wording.
public enum LibraryStaleness: Equatable, Sendable {
    case none
    /// Served from disk; a fetch (and a wake) is in flight.
    case waking
    /// Served from disk; the host did not answer.
    case offline

    /// The note the shelf shows, or nil when the titles are live.
    public var text: String? {
        switch self {
        case .none: return nil
        case .waking: return String(localized: L10n.libStaleWaking)
        case .offline: return String(localized: L10n.libStaleOffline)
        }
    }

    public var symbol: String {
        self == .waking ? "arrow.clockwise" : "wifi.slash"
    }
}

public extension LibraryGroupBy {
    /// The touch grid's stored grouping (`punktfunk.libraryGroupBy`): `platform`, `store`, or
    /// none for anything else.
    init?(stored: String) {
        switch stored {
        case "platform": self = .platform
        case "store": self = .store
        default: return nil
        }
    }
}
