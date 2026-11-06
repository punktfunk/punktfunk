// The Library tab's sections and the per-device layout that orders and hides them
// (design/apple-touch-ui-overhaul.md §2.5). Pure values, so the stored-string rules are tested:
// ids in order, a leading `-` for a section switched off, unknown ids dropped, and a section
// this build knows but the stored value lacks appended switched on.

import Foundation
import PunktfunkShared

/// One band of the Library tab. The raw value is the id stored in `punktfunk.librarySections`.
public enum LibrarySection: String, CaseIterable, Hashable, Sendable, Identifiable {
    case desktops
    case recent
    case favorites
    case launchers
    case games

    public var id: String { rawValue }

    public var label: String {
        switch self {
        case .desktops: return String(localized: L10n.libSectionDesktops)
        case .recent: return String(localized: L10n.libSectionRecent)
        case .favorites: return String(localized: L10n.libSectionFavorites)
        case .launchers: return String(localized: L10n.libSectionLaunchers)
        case .games: return String(localized: L10n.libSectionGames)
        }
    }

    /// The SF Symbol on the section's Customize row.
    public var symbol: String {
        switch self {
        case .desktops: return "desktopcomputer"
        case .recent: return "clock.arrow.circlepath"
        case .favorites: return "heart"
        case .launchers: return "square.stack"
        case .games: return "square.grid.2x2"
        }
    }
}

/// Which sections the Library tab shows, and in what order.
public struct LibrarySectionLayout: Equatable, Sendable {
    public struct Entry: Equatable, Sendable, Identifiable {
        public var section: LibrarySection
        public var isOn: Bool
        public var id: LibrarySection { section }

        public init(section: LibrarySection, isOn: Bool) {
            self.section = section
            self.isOn = isOn
        }
    }

    public var entries: [Entry]

    /// Parse the stored value. Lenient: an unknown id (a newer build's) drops, a repeated id
    /// keeps its first place, and a known section the value lacks is appended switched on.
    /// Empty or nil is every section in `allCases` order.
    public init(stored: String?) {
        var seen = Set<LibrarySection>()
        var entries: [Entry] = []
        for token in (stored ?? "").split(separator: ",") {
            let raw = token.trimmingCharacters(in: .whitespaces)
            let isOn = !raw.hasPrefix("-")
            guard let section = LibrarySection(rawValue: isOn ? raw : String(raw.dropFirst())),
                  seen.insert(section).inserted
            else { continue }
            entries.append(Entry(section: section, isOn: isOn))
        }
        for section in LibrarySection.allCases where !seen.contains(section) {
            entries.append(Entry(section: section, isOn: true))
        }
        self.entries = entries
    }

    /// The stored form: ids in order, `-` before a section switched off.
    public var stored: String {
        entries.map { ($0.isOn ? "" : "-") + $0.section.rawValue }.joined(separator: ",")
    }

    /// The sections to draw, in order.
    public var visible: [LibrarySection] {
        entries.filter(\.isOn).map(\.section)
    }
}
