// Sorting and grouping the library — the Swift port of `pf-console-ui`'s `collate.rs`, which is
// the portable SPEC every console shell implements: how titles fold for A–Z, what a platform-less
// Steam title buckets under, why launchers always lead, and when a library is worth browsing as
// collections at all. The rules are pinned by a shared vectors file rather than by prose:
// `clients/shared/library-collate-vectors.json` holds one mixed library and the exact groups each
// (sort, group_by) must produce, and `LibraryCollationTests` reads it — the desktop crate reads the
// same file, so the two copies cannot drift in silence. Desktop is the source of truth; a rule
// change lands there first, regenerates the file, and comes here second.
//
// Everything returns INDICES into the caller's array, never copies. The art cache, the running map
// and the cursor all key off the model's ordering, and a collation that handed back cloned games
// would fork the identity of every title on the shelf.
//
// Lives in PunktfunkKit rather than beside a view for the reason `LibraryGridNav` gives: pure
// values in, pure values out, and PunktfunkKit is the target the tests can reach.

import Foundation
import PunktfunkShared

/// How titles are ordered within a group. The raw value is the persisted `punktfunk.librarySort`
/// string — a FILE FORMAT shared with the desktop's `library_sort` (`host|title|platform|store`);
/// renaming one silently resets every user's chosen sort on their next launch.
public enum LibrarySortKey: String, CaseIterable, Hashable, Sendable {
    /// The host's own order, untouched. The default, and byte-identical to the shelf as it has
    /// always been — a user who never opens the sort pills must see no change at all.
    case hostOrder = "host"
    /// A–Z, case- and diacritic-relaxed, with the leading article folded away.
    case title = "title"
    /// Platform label A–Z, then title within it.
    case platform = "platform"
    /// Store label A–Z, then title.
    case store = "store"
    /// Newest launch first; titles never played keep host order after them.
    case recent = "recent"
    /// Most play time first; titles with none keep host order after them.
    case playTime = "playtime"

    /// Parse the persisted value. Lenient by design: an unknown string is a newer client's key,
    /// and the right answer to one is today's shelf rather than an error — the rule `uiPalette`
    /// follows too.
    public init(stored: String?) {
        self = LibrarySortKey(rawValue: stored ?? "") ?? .hostOrder
    }

    /// The persisted name.
    public var stored: String { rawValue }

    /// The pill's label.
    public var label: String {
        switch self {
        case .hostOrder: return String(localized: L10n.libSortHost)
        case .title: return String(localized: L10n.libSortTitle)
        case .platform: return String(localized: L10n.libSortPlatform)
        case .store: return String(localized: L10n.libSortStore)
        case .recent: return String(localized: L10n.libSortRecent)
        case .playTime: return String(localized: L10n.libSortPlaytime)
        }
    }

    /// The pills, in the order they are offered.
    public static let all: [LibrarySortKey] = [
        .hostOrder, .title, .platform, .store, .recent, .playTime,
    ]
}

/// The two arrangements of the gamepad library. Persisted as `punktfunk.libraryView`
/// (`shelf|grid`, the desktop's `library_view`); unknown → shelf.
public enum LibraryArrangement: String, CaseIterable, Hashable, Sendable {
    case shelf = "shelf"
    case grid = "grid"

    public init(stored: String?) {
        self = LibraryArrangement(rawValue: stored ?? "") ?? .shelf
    }

    public var stored: String { rawValue }

    public var label: String {
        switch self {
        case .shelf: return String(localized: L10n.playerLibraryViewShelf)
        case .grid: return String(localized: L10n.playerLibraryViewGrid)
        }
    }

    public static let all: [LibraryArrangement] = [.shelf, .grid]
}

/// What to bucket by. `nil` at the call sites = one group holding everything (the plain shelf).
public enum LibraryGroupBy: Hashable, Sendable {
    case platform
    case store
}

/// What a group IS, kept as data rather than a formatted string so a filtered library can be
/// compared against it without re-parsing a label.
public enum LibraryGroupKey: Hashable, Sendable {
    /// The launcher entries (design D4's leading group).
    case launchers
    case platform(String)
    case store(String)

    /// The platform name of the bucket for a game with neither platform nor known store. An id,
    /// never drawn: ``label`` words it.
    static let otherPlatform = "Other"

    /// The heading this group wears.
    public var label: String {
        switch self {
        case .launchers: return String(localized: L10n.libSectionLaunchers)
        case .platform(Self.otherPlatform): return String(localized: L10n.libGroupOther)
        case .platform(let name), .store(let name): return name
        }
    }

    /// The kind caption a collection tile wears (Launchers / Platform / Store).
    public var kindCaption: String {
        switch self {
        case .launchers: return String(localized: L10n.libCollectionKindLaunchers)
        case .platform: return String(localized: L10n.libCollectionKindPlatform)
        case .store: return String(localized: L10n.libCollectionKindStore)
        }
    }
}

/// One collated bucket: what it is, what to call it, and which games are in it.
public struct LibraryGroup: Hashable, Sendable {
    public let key: LibraryGroupKey
    public let label: String
    /// Indices into the array passed to `LibraryCollation.collate`, in display order.
    public let indices: [Int]

    public init(key: LibraryGroupKey, label: String, indices: [Int]) {
        self.key = key
        self.label = label
        self.indices = indices
    }
}

public enum LibraryCollation {
    /// The desktop tile's id — the synthetic entry every shelf leads with, so Library is never a
    /// dead end for the desktop-only user and a plugin-less host is still one press from
    /// streaming. The `\0` prefix is the desktop console's (`pf-console-ui`'s `DESKTOP_ID`): a
    /// host title id is a store reference, and none of them can start with a NUL.
    ///
    /// Presentation only. It is never persisted, never fetched, and never grouped.
    public static let desktopID = "\u{0}desktop"

    /// The mark that tile draws (`pf-client-core`'s `DESKTOP_ICON`). A Lucide name on the shells
    /// that carry that set; `launcherIconImage` maps it to this platform's own symbol.
    public static let desktopIcon = "monitor"

    /// The synthetic tile itself. No art and an empty store: nothing tries to fetch a poster for
    /// it, and the shelf draws the mark above instead of a monogram.
    public static func desktopEntry(title: String = String(localized: L10n.libDesktop)) -> GameEntry {
        GameEntry(id: desktopID, store: "", title: title, art: Artwork(), icon: desktopIcon)
    }

    /// Fold a title down to something sortable: lowercase, diacritics relaxed to their base
    /// letter, punctuation dropped, and a leading article removed.
    ///
    /// The article fold is what a user actually means by A–Z. "The Witcher 3" belongs under W;
    /// left alone, every "The …" in a library piles up under T and the sort is useless exactly
    /// where it is most needed. English articles only — the host's titles are whatever the store
    /// called them, and inventing rules for languages we cannot detect would file things under
    /// letters nobody expects.
    public static func sortTitle(_ title: String) -> String {
        var relaxed = String.UnicodeScalarView()
        for scalar in title.lowercased().unicodeScalars {
            let folded = fold(scalar)
            let props = folded.properties
            // Rust's `is_alphanumeric() || is_whitespace()`: Alphabetic, or a numeric type,
            // or White_Space — punctuation and symbols go.
            if props.isAlphabetic || props.numericType != nil || props.isWhitespace {
                relaxed.append(folded)
            }
        }
        let trimmed = String(relaxed).trimmingCharacters(in: .whitespacesAndNewlines)
        for article in ["the ", "a ", "an "] where trimmed.hasPrefix(article) {
            // Guard against a title that IS an article ("The", "A Way Out" keeps "way out", but a
            // bare "The" must not sort as an empty string and float to the front).
            let rest = String(trimmed.dropFirst(article.count))
                .trimmingCharacters(in: .whitespacesAndNewlines)
            if !rest.isEmpty { return rest }
        }
        return trimmed
    }

    /// `sortTitle`, remembered: a shelf collates on every redraw, and its titles don't change.
    /// 4096 titles is past any library; a full table starts over.
    private static func sortKey(_ title: String) -> String {
        sortKeyLock.withLock {
            if let known = sortKeys[title] { return known }
            if sortKeys.count >= 4096 { sortKeys.removeAll(keepingCapacity: true) }
            let key = sortTitle(title)
            sortKeys[title] = key
            return key
        }
    }

    private static let sortKeyLock = NSLock()
    nonisolated(unsafe) private static var sortKeys: [String: String] = [:]

    /// The desktop's fold table, scalar for scalar.
    private static func fold(_ scalar: Unicode.Scalar) -> Unicode.Scalar {
        switch scalar {
        case "á", "à", "â", "ä", "ã", "å": return "a"
        case "é", "è", "ê", "ë": return "e"
        case "í", "ì", "î", "ï": return "i"
        case "ó", "ò", "ô", "ö", "õ": return "o"
        case "ú", "ù", "û", "ü": return "u"
        case "ç": return "c"
        case "ñ": return "n"
        default: return scalar
        }
    }

    /// The bucket for one game under `by`.
    ///
    /// The interesting case is a game with no platform. It does NOT go to "Unknown": a Steam
    /// library is entirely platform-less, and one giant "Unknown" heap would be a worse view than
    /// no grouping at all. A store-front game buckets under its STORE instead ("Steam"), which is
    /// both true and useful, and only an entry with neither lands in "Other".
    static func bucket(_ game: GameEntry, by: LibraryGroupBy) -> LibraryGroupKey {
        switch by {
        case .platform:
            let platform = game.platform?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            if !platform.isEmpty { return .platform(platform) }
            return game.hasStoreName ? .store(game.storeLabel) : .platform(LibraryGroupKey.otherPlatform)
        case .store:
            return .store(game.storeLabel)
        }
    }

    /// Rust's `str::cmp` — bytewise over UTF-8, so `"GOG" < "Game" < "Steam"`. Every ordering
    /// in this file goes through it: a locale-aware `<` would file the same library differently
    /// on two devices, and differently from the desktop.
    static func bytewise(_ a: String, _ b: String) -> Bool {
        a.utf8.lexicographicallyPrecedes(b.utf8)
    }

    /// Collate `games` into display groups.
    ///
    /// Launchers always form the leading group, which is how design D4's "launcher entries come
    /// first" invariant survives grouping BY CONSTRUCTION rather than by every caller remembering
    /// it. Sorting applies WITHIN groups, never across them.
    public static func collate(
        _ games: [GameEntry], sort: LibrarySortKey, groupBy: LibraryGroupBy?
    ) -> [LibraryGroup] {
        var launchers: [Int] = []
        // Insertion-ordered rather than a dictionary, so groups appear in the order the library
        // first mentions them and two runs over the same library agree.
        var buckets: [(key: LibraryGroupKey, indices: [Int])] = []
        for (i, game) in games.enumerated() {
            // The desktop tile is not a title: grouping it would put a "Desktop" platform in
            // Collections and make a one-store library look browsable. `filtered` puts it back
            // at the head of an unfiltered shelf, which is the only place it belongs.
            if game.id == LibraryCollation.desktopID { continue }
            if game.isLauncher {
                launchers.append(i)
                continue
            }
            // Ungrouped: one bucket holding the whole shelf. Its label is never drawn (the shelf
            // has no heading when there is only one group), so it names itself honestly rather
            // than inventing a title.
            let key = groupBy.map { bucket(game, by: $0) } ?? .platform("All")
            if let at = buckets.firstIndex(where: { $0.key == key }) {
                buckets[at].indices.append(i)
            } else {
                buckets.append((key, [i]))
            }
        }

        // Fold every title once; the comparator below runs O(n log n) times per group. Only
        // these three sorts read a title.
        let readsTitles = sort == .title || sort == .platform || sort == .store
        let titleKeys = readsTitles ? games.map { sortKey($0.title) } : []
        // Every comparator falls back to the index, so equal keys keep the host's order rather
        // than an arbitrary one — and the result is the same whether or not `sorted` is stable.
        func precedes(_ a: Int, _ b: Int) -> Bool {
            let byTitleThenIndex: () -> Bool = {
                if titleKeys[a] != titleKeys[b] { return bytewise(titleKeys[a], titleKeys[b]) }
                return a < b
            }
            switch sort {
            // Untouched: the host's order IS the order.
            case .hostOrder:
                return a < b
            case .title:
                return byTitleThenIndex()
            case .platform:
                let pa = games[a].platform ?? "", pb = games[b].platform ?? ""
                if pa != pb { return bytewise(pa, pb) }
                return byTitleThenIndex()
            case .store:
                let sa = games[a].storeLabel, sb = games[b].storeLabel
                if sa != sb { return bytewise(sa, sb) }
                return byTitleThenIndex()
            case .recent:
                let la = games[a].stats?.lastPlayedUnixMs ?? 0
                let lb = games[b].stats?.lastPlayedUnixMs ?? 0
                if la != lb { return la > lb }
                return a < b
            case .playTime:
                let pa = games[a].stats?.playTimeMs ?? 0
                let pb = games[b].stats?.playTimeMs ?? 0
                if pa != pb { return pa > pb }
                return a < b
            }
        }

        var out: [LibraryGroup] = []
        if !launchers.isEmpty {
            // Launchers keep the host's order whatever the sort says: there are two or three of
            // them, they are a fixed set, and shuffling them by title makes muscle memory useless
            // for no gain.
            out.append(LibraryGroup(key: .launchers, label: LibraryGroupKey.launchers.label, indices: launchers))
        }
        // Groups themselves go A–Z by label, launchers excepted (pinned first); ties keep insertion
        // order, exactly like the desktop's stable sort.
        let ordered = buckets.enumerated().sorted { a, b in
            if a.element.key.label != b.element.key.label {
                return bytewise(a.element.key.label, b.element.key.label)
            }
            return a.offset < b.offset
        }
        for (_, bucket) in ordered {
            out.append(LibraryGroup(
                key: bucket.key, label: bucket.key.label, indices: bucket.indices.sorted(by: precedes)))
        }
        return out
    }

    /// The flat index list for a group filter — `nil` = the whole library, in collated order
    /// (launchers included, first).
    public static func filtered(
        _ games: [GameEntry], sort: LibrarySortKey, filter: LibraryGroupKey?
    ) -> [Int] {
        let by: LibraryGroupBy?
        switch filter {
        case .platform: by = .platform
        case .store: by = .store
        case .launchers, .none: by = nil
        }
        let groups = collate(games, sort: sort, groupBy: by)
        guard let want = filter else {
            // Ahead of the launchers, and ahead of every sort: it is the host itself.
            let desktop = games.firstIndex { $0.id == LibraryCollation.desktopID }
            return (desktop.map { [$0] } ?? []) + groups.flatMap(\.indices)
        }
        // A filter naming a group that no longer exists yields NOTHING, never everything.
        return groups.first { $0.key == want }?.indices ?? []
    }

    /// Is there anything worth browsing? A library with one platform and one store has nothing to
    /// drill INTO, and a screen that opens onto a single tile is worse than no screen — so the
    /// button that reaches it is hidden rather than made to disappoint. ⚠ A Steam-only library
    /// with no platform metadata is ONE group and is not worth browsing — by design.
    public static func worthBrowsing(_ games: [GameEntry]) -> Bool {
        collate(games, sort: .hostOrder, groupBy: .platform)
            .filter { $0.key != .launchers }
            .count >= 2
    }
}

/// The shelf's display order — the one rule every writer of the catalog goes through, the
/// desktop's `order()`.
///
/// Two rules, in this order:
/// 1. **Launcher entries lead** (design D4). Applied FIRST, because a grid's layout is built on
///    it: the launcher rows sit above the game rows as a prefix. Let a running game jump ahead of
///    a launcher and the heading reads GAMES · LAUNCHERS · GAMES along the strip.
/// 2. **Running titles lead within their group.** Getting back into what is already up should
///    be the first thing on the shelf rather than something to scroll for — and a launcher that
///    is up still belongs with the launchers, which is what makes the two rules compose instead
///    of fighting.
///
/// A stable partition into four bands; the host's own title order survives inside each.
public enum LibraryOrder {
    public static func display(_ games: [GameEntry], running: Set<String>) -> [GameEntry] {
        games.enumerated().sorted { a, b in
            // The desktop tile leads every band: it is the host, not one of its titles.
            let ka = (
                a.element.id == LibraryCollation.desktopID ? 0 : 1,
                a.element.isLauncher ? 0 : 1, running.contains(a.element.id) ? 0 : 1
            )
            let kb = (
                b.element.id == LibraryCollation.desktopID ? 0 : 1,
                b.element.isLauncher ? 0 : 1, running.contains(b.element.id) ? 0 : 1
            )
            if ka != kb { return ka < kb }
            return a.offset < b.offset
        }.map(\.element)
    }
}
