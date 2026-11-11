// LaunchedEntry: the tap a launch connect raises its hold from, keyed by id.

import CoreGraphics
import XCTest

@testable import PunktfunkKit

final class LaunchedEntryTests: XCTestCase {
    private let dota = GameEntry(id: "steam:570", store: "steam", title: "Dota 2", art: Artwork())
    private let tile = CGRect(x: 10, y: 20, width: 120, height: 180)

    override func tearDown() {
        LaunchedEntry.remember(nil, from: nil)
        super.tearDown()
    }

    /// The connect for the tapped id takes the entry and its tile, once; a peek leaves it.
    func testTheTappedLaunchTakesItsEntryOnce() {
        LaunchedEntry.remember(dota, from: tile)
        XCTAssertEqual(LaunchedEntry.peek("steam:570"), dota)
        let taken = LaunchedEntry.take("steam:570")
        XCTAssertEqual(taken?.entry, dota)
        XCTAssertEqual(taken?.rect, tile)
        XCTAssertNil(LaunchedEntry.take("steam:570"))
        XCTAssertNil(LaunchedEntry.peek("steam:570"))
    }

    /// Another id never wears the entry.
    func testAnotherLaunchGetsNothing() {
        LaunchedEntry.remember(dota, from: nil)
        XCTAssertNil(LaunchedEntry.peek("steam:1"))
        XCTAssertNil(LaunchedEntry.take("steam:1"))
    }
}
