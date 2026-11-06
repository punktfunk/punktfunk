// How the host grid is ordered and grouped.
//
// Pure value logic, deliberately: which card lands where is the kind of thing that reads as
// obviously right and behaves subtly wrong (an unstable sort reshuffling equal rows on every
// redraw, a never-connected host sorting as if it were connected in 1970), so it lives apart from
// the view and is tested. It sits in PunktfunkShared because everything it touches — `StoredHost`,
// `PresetCatalog` — already does.
//
// It arranges CARDS, not hosts. A host contributes its own card plus one per pinned preset
// (§5.2a), and under preset grouping those go to different bands — the pinned card belongs to
// the preset it connects with, which is the whole reason it exists.

import Foundation

/// The order hosts appear in.
public enum HostSort: String, CaseIterable, Identifiable, Sendable {
    /// Oldest first — the order they were added, and what the grid did before it could sort at
    /// all. The default for exactly that reason: an update shouldn't rearrange anyone's grid.
    case added
    case name
    case lastConnected

    public var id: String { rawValue }

    public var label: String {
        switch self {
        case .added: return String(localized: L10n.playerHostSortAdded)
        case .name: return String(localized: L10n.playerHostSortName)
        case .lastConnected: return String(localized: L10n.playerHostSortLastConnected)
        }
    }

    public var symbol: String {
        switch self {
        case .added: return "calendar"
        case .name: return "textformat.abc"
        case .lastConnected: return "clock.arrow.circlepath"
        }
    }
}

/// What the grid is divided by.
public enum HostGrouping: String, CaseIterable, Identifiable, Sendable {
    case none
    /// The preset each CARD connects with: a host's own card follows its binding, a pinned card
    /// follows its pin. Grouping by the binding alone would file every pinned card under "No
    /// preset" — pins are not bindings, and a pinned card is precisely the one that knows which
    /// preset it means. Stored as its raw value, which `init(rawValue:)` also reads under its
    /// pre-rename spelling.
    case preset
    case status

    public init?(rawValue: String) {
        switch rawValue {
        case "none": self = .none
        case "preset", "profile": self = .preset
        case "status": self = .status
        default: return nil
        }
    }

    public var id: String { rawValue }

    public var label: String {
        switch self {
        case .none: return String(localized: L10n.playerHostGroupingNone)
        case .preset: return String(localized: L10n.playerHostGroupingPreset)
        case .status: return String(localized: L10n.playerHostGroupingStatus)
        }
    }

    public var symbol: String {
        switch self {
        case .none: return "square.grid.2x2"
        case .preset: return "slider.horizontal.3"
        case .status: return "wifi"
        }
    }
}

/// One card in the grid: a host, and the preset that card connects with.
public struct HostCard: Identifiable, Equatable, Sendable {
    public let host: StoredHost
    /// nil = the host's own card, which follows whatever it is bound to. Set = a pinned
    /// host+preset card (§5.2a).
    public let pinned: StreamPreset?

    public var id: String { "\(host.id.uuidString)#\(pinned?.id ?? "")" }

    public init(host: StoredHost, pinned: StreamPreset? = nil) {
        self.host = host
        self.pinned = pinned
    }
}

/// One rendered band of the grid. `title` is nil for the ungrouped case, which is what tells the
/// view to draw no header at all rather than an empty one.
public struct HostGroup: Identifiable, Equatable, Sendable {
    public let id: String
    public let title: String?
    /// `#RRGGBB` when the band is a preset — its chip colour, so the header matches the cards.
    public let accent: String?
    public let cards: [HostCard]

    public init(id: String, title: String?, accent: String? = nil, cards: [HostCard]) {
        self.id = id
        self.title = title
        self.accent = accent
        self.cards = cards
    }
}

public enum HostArrangement {
    /// Group, then sort within each group. `online` is the set of host ids currently reachable —
    /// the caller owns that (mDNS plus the reachability probe), this only reads it.
    public static func groups(
        hosts: [StoredHost], catalog: PresetCatalog, online: Set<UUID>,
        sort: HostSort, grouping: HostGrouping
    ) -> [HostGroup] {
        // The original index is the tie-break for every sort below. `Array.sorted` is NOT stable
        // in Swift, so without it two hosts that compare equal — same name, both never connected —
        // could swap places between redraws.
        let ranked = hosts.enumerated().map { (index: $0.offset, host: $0.element) }

        func ordered(_ slice: [(index: Int, host: StoredHost)]) -> [StoredHost] {
            slice.sorted { lhs, rhs in
                switch sort {
                case .added:
                    // Undated hosts first: no date means saved before the field existed, which
                    // means older than anything carrying one. In a real store those are a PREFIX
                    // (everything from before the upgrade), so they hold their stored order and
                    // the grid looks exactly as it did — which is why this is the default sort.
                    let a = lhs.host.addedAt, b = rhs.host.addedAt
                    if let a, let b, a != b { return a < b }
                    if (a == nil) != (b == nil) { return a == nil }
                case .name:
                    let comparison = lhs.host.displayName.localizedStandardCompare(
                        rhs.host.displayName)
                    if comparison != .orderedSame { return comparison == .orderedAscending }
                case .lastConnected:
                    // Never-connected hosts go last, not first — `.distantPast` would put a host
                    // you have never used at the top of "Last Connected".
                    switch (lhs.host.lastConnected, rhs.host.lastConnected) {
                    case let (a?, b?) where a != b: return a > b
                    case (nil, _?): return false
                    case (_?, nil): return true
                    default: break
                    }
                }
                return lhs.index < rhs.index
            }.map(\.host)
        }

        /// A host's own card, then one per pinned preset — so a pin reads as belonging to its
        /// host wherever the two sit together.
        func cards(_ host: StoredHost) -> [HostCard] {
            [HostCard(host: host)] + catalog.pinned(for: host).map { HostCard(host: host, pinned: $0) }
        }

        switch grouping {
        case .none:
            return [HostGroup(id: "all", title: nil, cards: ordered(ranked).flatMap(cards))]

        case .preset:
            // Catalog order, so the bands sit in the same order the scope menu lists them, then
            // whatever belongs to no preset. A band with nothing in it isn't drawn.
            let sorted = ordered(ranked)
            var groups: [HostGroup] = catalog.presets.compactMap { preset in
                let members: [HostCard] = sorted.flatMap { host -> [HostCard] in
                    var found: [HostCard] = []
                    // A host BOUND to this preset brings its own card…
                    if catalog.binding(for: host)?.id == preset.id {
                        found.append(HostCard(host: host))
                    }
                    // …and a host that PINNED it brings that pinned card, whatever it is bound
                    // to. Both can be true: a bound preset may also be pinned.
                    if let pin = catalog.pinned(for: host).first(where: { $0.id == preset.id }) {
                        found.append(HostCard(host: host, pinned: pin))
                    }
                    return found
                }
                guard !members.isEmpty else { return nil }
                return HostGroup(
                    id: "preset-\(preset.id)", title: preset.name, accent: preset.accent,
                    cards: members)
            }
            // Hosts that a plain tap streams with the defaults. A dangling binding resolves as
            // none (§4.4), so it lands here rather than in a band named after a preset that no
            // longer exists.
            let unbound = sorted.filter { catalog.binding(for: $0) == nil }.map { HostCard(host: $0) }
            if !unbound.isEmpty {
                groups.append(
                    HostGroup(
                        id: "preset-none", title: String(localized: L10n.hostsGroupNoPreset),
                        cards: unbound))
            }
            return groups

        case .status:
            var groups: [HostGroup] = []
            let up = ranked.filter { online.contains($0.host.id) }
            let down = ranked.filter { !online.contains($0.host.id) }
            if !up.isEmpty {
                groups.append(
                    HostGroup(id: "status-online", title: String(localized: L10n.hostsGroupOnline),
                              cards: ordered(up).flatMap(cards)))
            }
            if !down.isEmpty {
                groups.append(
                    HostGroup(id: "status-offline", title: String(localized: L10n.hostsGroupOffline),
                              cards: ordered(down).flatMap(cards)))
            }
            return groups
        }
    }
}
