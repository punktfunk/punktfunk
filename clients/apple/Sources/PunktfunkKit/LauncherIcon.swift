// The launcher tiles' brand marks: template vector imagesets in Resources/LauncherIcons.xcassets
// (generated from the repo's assets/launcher-icons masters by scripts/gen-launcher-icons.sh —
// per-mark provenance and licensing in that directory's README), resolved from a library entry's
// `icon` token. Template rendering means they tint with `foregroundStyle` like an SF Symbol.
//
// The sibling of OsIcon.swift, which does the equivalent job for the host cards' OS marks. SF
// Symbols ships no third-party brand glyphs, so a curated registry is the only route.

import SwiftUI

/// The icon tokens this client ships art for. A token outside this set draws nothing and the tile
/// falls back to naming its launcher — which is what every launcher tile looked like before icons
/// existed, so an unknown mark degrades to the old design rather than to a hole.
///
/// Checked against rather than interpolated: `Image(named:)` is a name lookup, and the set is the
/// only thing that decides which names it can ever see.
private let launcherIconTokensShipped: Set<String> = [
    "steam", "lutris", "heroic", "playnite", "epic", "gog", "xbox", "hydra",
]

/// Tokens that are not brands: a UI mark the other shells take from Lucide, which here is the
/// nearest SF Symbol. `monitor` is the desktop tile's (`LibraryCollation.desktopIcon`).
private let systemSymbolForToken: [String: String] = ["monitor": "desktopcomputer"]

/// The mark for a library entry's `icon` token, or nil — no view at all — when the entry carries
/// no token or names one this client ships no art for.
public func launcherIconImage(for token: String?) -> Image? {
    guard let token else { return nil }
    if let symbol = systemSymbolForToken[token] { return Image(systemName: symbol) }
    guard launcherIconTokensShipped.contains(token) else { return nil }
    return Image("launcher-\(token)", bundle: .module)
}
