import SwiftUI
import XCTest

import PunktfunkShared

/// The generated catalog loads from `PunktfunkShared`'s bundle and picks a plural's form. The
/// accessor's default value is the English `other` text, so a `one` form proves the lookup.
final class L10nTests: XCTestCase {
    func testPlainMessage() {
        XCTAssertEqual(String(localized: L10n.playerStatsScalePct), "Statistics size")
        XCTAssertEqual(
            String(localized: L10n.playerStatsScalePctHint),
            "The overlay's size, on top of your display's scaling.")
    }

    func testPluralPicksTheForm() {
        XCTAssertEqual(L10n.prefsAdvancedChanged(count: 1), "1 advanced setting changed")
        XCTAssertEqual(L10n.prefsAdvancedChanged(count: 3), "3 advanced settings changed")
    }

    /// No variant shows the count, and the count is still the selector.
    func testPluralWithoutTheNumber() {
        XCTAssertEqual(L10n.overlayPlayers(count: 1, players: "1"), "player 1")
        XCTAssertEqual(L10n.overlayPlayers(count: 2, players: "1, 2"), "players 1, 2")
    }

    /// `Text` reads Markdown in a resource, its arguments included, and none in a `String` or in
    /// a literal's arguments. So a message with arguments is a `String`.
    @MainActor func testTextReadsMarkdownInAResourceOnly() {
        func width(_ text: Text) -> Int {
            ImageRenderer(content: text.font(.system(size: 20)).fixedSize()).cgImage?.width ?? 0
        }
        let name = "a *b* c"
        let resource = LocalizedStringResource("l10n_tests_absent", defaultValue: "on \(name)")
        XCTAssertLessThan(width(Text(resource)), width(Text(verbatim: "on \(name)")))
        XCTAssertEqual(width(Text("on \(name)")), width(Text(verbatim: "on \(name)")))
        XCTAssertEqual(width(Text(String(localized: resource))), width(Text(verbatim: "on \(name)")))
    }
}
