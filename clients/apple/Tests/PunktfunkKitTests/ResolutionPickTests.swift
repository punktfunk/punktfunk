// The settings resolution wheel and the TV's stream-mode row.

import XCTest

@testable import PunktfunkShared

final class ResolutionPickTests: XCTestCase {
    private let tags: Set<String> = ["0x0", "1280x720", "1920x1080", "2560x1440"]

    func testAWheelSizeIsNotCustom() {
        XCTAssertFalse(ResolutionPick.isCustom(customMode: false, width: 1920, height: 1080, tags: tags))
        XCTAssertFalse(ResolutionPick.isCustom(customMode: false, width: 0, height: 0, tags: tags))
    }

    func testASizeOffTheWheelIsCustomWithoutTheFlag() {
        XCTAssertTrue(ResolutionPick.isCustom(customMode: false, width: 1500, height: 1000, tags: tags))
    }

    func testTheFlagHoldsCustomOverAWheelSize() {
        XCTAssertTrue(ResolutionPick.isCustom(customMode: true, width: 1920, height: 1080, tags: tags))
    }

    func testCustomOverNativeOpensOn1080p() {
        let pick = ResolutionPick.apply(tag: ResolutionPick.customTag, currentWidth: 0)
        XCTAssertTrue(pick.customMode)
        XCTAssertEqual(pick.size?.w, 1920)
        XCTAssertEqual(pick.size?.h, 1080)
    }

    func testCustomOverASizeKeepsIt() {
        let pick = ResolutionPick.apply(tag: ResolutionPick.customTag, currentWidth: 2560)
        XCTAssertTrue(pick.customMode)
        XCTAssertNil(pick.size)
    }

    func testAWheelRowWritesItsSizeAndLeavesCustom() {
        let pick = ResolutionPick.apply(tag: "2560x1440", currentWidth: 1500)
        XCTAssertFalse(pick.customMode)
        XCTAssertEqual(pick.size?.w, 2560)
        XCTAssertEqual(pick.size?.h, 1440)
        let native = ResolutionPick.apply(tag: ResolutionPick.tag(width: 0, height: 0), currentWidth: 1920)
        XCTAssertEqual(native.size?.w, 0)
    }

    func testAnUnreadableTagWritesNothing() {
        let pick = ResolutionPick.apply(tag: "wide", currentWidth: 1920)
        XCTAssertFalse(pick.customMode)
        XCTAssertNil(pick.size)
    }

    func testStreamModeTagsRoundTrip() {
        let tag = ResolutionPick.modeTag(width: 1920, height: 1080, hz: 60)
        XCTAssertEqual(tag, "1920x1080x60")
        let mode = ResolutionPick.mode(tag: tag)
        XCTAssertEqual(mode?.w, 1920)
        XCTAssertEqual(mode?.h, 1080)
        XCTAssertEqual(mode?.hz, 60)
        XCTAssertNil(ResolutionPick.mode(tag: "1920x1080"))
    }

    func testTheStreamModeRowsMarkerCoversItsSize() {
        var overlay = SettingsOverlay()
        overlay.width = 1920
        overlay.height = 1080
        XCTAssertTrue(ResolutionPick.modeFields.contains { OverlayField.isOverridden($0, in: overlay) })
    }

    /// R6-2: the TV's mode row writes a size and a rate, and its marker and Reset covered the size
    /// alone, so a preset kept a rate pin nothing on the TV showed or cleared.
    func testTheStreamModeRowsResetClearsTheRateItWrites() throws {
        let mode = try XCTUnwrap(ResolutionPick.mode(tag: "1920x1080x60"))
        var overlay = SettingsOverlay()
        overlay.width = mode.w
        overlay.height = mode.h
        overlay.refreshHz = mode.hz
        for field in ResolutionPick.modeFields { OverlayField.clear(field, in: &overlay) }
        XCTAssertEqual(overlay, SettingsOverlay())

        var rateOnly = SettingsOverlay()
        rateOnly.refreshHz = 60
        XCTAssertTrue(ResolutionPick.modeFields.contains { OverlayField.isOverridden($0, in: rateOnly) })
    }
}
