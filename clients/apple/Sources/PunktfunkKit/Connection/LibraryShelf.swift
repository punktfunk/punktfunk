// The touch shelf's decisions: which titles each of its rows holds. Pure values over
// `LibraryCollation`'s rules, so `LibraryShelfTests` pins them; `LibraryView` gathers the facts
// and draws the result.

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
