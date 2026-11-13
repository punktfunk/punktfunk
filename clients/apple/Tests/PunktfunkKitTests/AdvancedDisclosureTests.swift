// A settings category's advanced rows: shown, counted behind a button, or not there.

import XCTest

@testable import PunktfunkShared

final class AdvancedDisclosureTests: XCTestCase {
    private func state(
        show: Bool = false, overridden: Bool = false, changed: Int = 0, preset: Bool = false
    ) -> AdvancedDisclosure {
        AdvancedDisclosure.state(
            showAdvanced: show, overridden: overridden, changed: changed, inPreset: preset)
    }

    func testShowAdvancedListsTheRows() {
        XCTAssertEqual(state(show: true), .rows)
        XCTAssertEqual(state(show: true, changed: 3), .rows)
        XCTAssertEqual(state(show: true, preset: true), .rows)
    }

    func testAPresetOverridingOneListsTheRows() {
        XCTAssertEqual(state(overridden: true, preset: true), .rows)
    }

    func testDefaultsCountTheirChangedRows() {
        XCTAssertEqual(state(changed: 2), .countButton(2))
    }

    func testNothingChangedShowsNothing() {
        XCTAssertEqual(state(), .hidden)
    }

    func testAPresetOffersNoCount() {
        XCTAssertEqual(state(changed: 2, preset: true), .hidden)
    }
}
