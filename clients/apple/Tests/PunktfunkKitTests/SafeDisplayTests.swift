// The safe-area stream mode (SafeDisplay), as pure geometry: Moonlight's formula — full native
// height, width reduced by the left+right safe insets — plus the host's dimension rules (even, and
// never under 320×200) and the landscape-inset resolution that makes the row correct even when the
// settings screen it is rendered on is currently portrait.

import XCTest

import PunktfunkShared
@testable import PunktfunkKit

final class SafeDisplayTests: XCTestCase {
    func testLandscapeUsesTheHorizontalInsets() {
        // Landscape: the housing is on a side and iOS symmetrizes the two, so either one is the
        // per-side inset.
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 59, right: 59, top: 0, isPhone: true), 59)
        // Unequal readings on both sides reduce to the larger — never under-inset.
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 44, right: 59, top: 0, isPhone: true), 59)
    }

    func testAOneSidedInsetIsABarNotAHousing() {
        // A foldable's cover screen: the side bar insets one edge by 84 pt, in either orientation,
        // and its top inset is zero. Nothing there hides a full-screen stream.
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 0, right: 84, top: 0, isPhone: true), 0)
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 84, right: 0, top: 0, isPhone: true), 0)
    }

    func testPortraitFallsBackToTheHousingTopInset() {
        // Portrait on a notched phone: left/right are zero and the housing sits on `top`. Reading
        // the horizontal insets here would compute "no inset" for exactly the devices that need one,
        // so the portrait top inset stands in — it is the same physical intrusion.
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 0, right: 0, top: 59, isPhone: true), 59)
        // A plain status bar is not a housing: an iPad (or a pre-notch iPhone) must not fabricate an
        // inset for a device with nothing to route around.
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 0, right: 0, top: 24, isPhone: true), 0)
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 0, right: 0, top: 59, isPhone: false), 0)
    }

    func testModeInsetsWidthOnlyAndKeepsFullHeight() {
        // A Dynamic Island phone: 2556×1179 native, 59 pt per side at nativeScale 3 → 177 px per
        // side, 354 px total. Height is untouched — under aspect-fit only the horizontal axis binds.
        let m = SafeDisplay.mode(
            nativeWidth: 2556, nativeHeight: 1179, sideInsetPoints: 59, scale: 3)
        XCTAssertEqual(m.width, 2202, "2556 − 2×177")
        XCTAssertEqual(m.height, 1178, "odd native heights even-floor")
        // The safe mode must be NARROWER than native, or it would still fill the housing.
        XCTAssertLessThan(m.width, 2556)
    }

    func testNoHousingYieldsTheNativeModeSoTheRowDedups() {
        // Zero inset ⇒ identical to native (bar the even-floor). `resolutionModes` dedups by
        // dimensions, so this is what makes the extra row vanish on a device that has no housing
        // rather than showing a pointless duplicate.
        let m = SafeDisplay.mode(
            nativeWidth: 2360, nativeHeight: 1640, sideInsetPoints: 0, scale: 2)
        XCTAssertEqual(m.width, 2360)
        XCTAssertEqual(m.height, 1640)
    }

    func testResultIsAlwaysHostValid() {
        // Odd widths even-floor: `validate_dimensions` rejects odd outright, and an inset
        // subtraction lands odd about half the time.
        let odd = SafeDisplay.mode(
            nativeWidth: 2001, nativeHeight: 1001, sideInsetPoints: 0, scale: 1)
        XCTAssertEqual(odd.width % 2, 0)
        XCTAssertEqual(odd.height % 2, 0)
        // An absurd inset can't drive the mode under the host's floor.
        let tiny = SafeDisplay.mode(
            nativeWidth: 1280, nativeHeight: 720, sideInsetPoints: 5000, scale: 3)
        XCTAssertEqual(tiny.width, SafeDisplay.minWidth)
        XCTAssertEqual(tiny.height, 720)
        // A negative inset is treated as none rather than widening past the panel.
        let neg = SafeDisplay.mode(
            nativeWidth: 1280, nativeHeight: 720, sideInsetPoints: -40, scale: 3)
        XCTAssertEqual(neg.width, 1280)
    }

    func testMacModeInsetsHeightOnly() {
        // A 13" MacBook Air at native scaling: 2560×1664 panel, a 32 pt housing over 832 pt of
        // height ⇒ 64 px off the top, the 2560×1600 a full-screen stream shows whole.
        let m = SafeDisplay.mode(
            nativeWidth: 2560, nativeHeight: 1664, topInsetPoints: 32, scale: 2)
        XCTAssertEqual(m.width, 2560, "the housing binds the vertical axis only")
        XCTAssertEqual(m.height, 1600, "1664 − 64")
    }

    func testMacScaledModeStillLandsOnWholePixels() {
        // The same panel driven "More Space": 1710×1112 pt, so the point→pixel factor is a ratio
        // (1664/1112) and the housing lands a hair off 64 px. Rounding first is what keeps the
        // answer 1600 — the raw 64.05 would even-floor to 1598, two pixels of panel dropped.
        let m = SafeDisplay.mode(
            nativeWidth: 2560, nativeHeight: 1664,
            topInsetPoints: 42.8, scale: 1664.0 / 1112.0)
        XCTAssertEqual(m.height, 1600)
    }

    func testMacModeStopsAtTheMenuBarNotTheHousing() {
        // A 14" MacBook Pro at 2×: a 32 pt housing in a 37 pt band. 1900 rows in the 1890 pt×2
        // fullscreen window would miss the 1:1 snap and resample the whole picture.
        let pro14 = SafeDisplay.mode(
            nativeWidth: 3024, nativeHeight: 1964,
            topInsetPoints: SafeDisplay.fullscreenTopInset(housing: 32, menuBar: 37), scale: 2)
        XCTAssertEqual(pro14.height, 1890)

        // A 16" at 2056×1329 pt, measured: 38 pt housing, 39 pt band, a 1290 pt window (2168.4 px).
        let pro16 = SafeDisplay.mode(
            nativeWidth: 3456, nativeHeight: 2234,
            topInsetPoints: SafeDisplay.fullscreenTopInset(housing: 38, menuBar: 39),
            scale: 2234.0 / 1329.0)
        XCTAssertEqual(pro16.height, 2168)

        XCTAssertEqual(
            SafeDisplay.fullscreenTopInset(housing: 32, menuBar: 0), 32, "a hidden menu bar")
        XCTAssertEqual(
            SafeDisplay.fullscreenTopInset(housing: 0, menuBar: 24), 0, "no housing, whole panel")
    }

    func testOnlyAPictureTallerThanTheSafeAreaCoversTheHousing() {
        // A 13" Air: 2560×1664 panel, 2560×1600 below the housing.
        let panel = (width: 2560, height: 1664), safe = (width: 2560, height: 1600)
        XCTAssertTrue(SafeDisplay.coversHousing(content: (2560, 1664), panel: panel, safe: safe))
        XCTAssertTrue(SafeDisplay.coversHousing(content: (1600, 1200), panel: panel, safe: safe))
        // These fit the safe area whole, so the housing would only black out rows.
        XCTAssertFalse(SafeDisplay.coversHousing(content: (2560, 1600), panel: panel, safe: safe))
        XCTAssertFalse(SafeDisplay.coversHousing(content: (1920, 1200), panel: panel, safe: safe))
        XCTAssertFalse(SafeDisplay.coversHousing(content: (2560, 1440), panel: panel, safe: safe))
        // No mode yet, or no housing.
        XCTAssertFalse(SafeDisplay.coversHousing(content: (0, 0), panel: panel, safe: safe))
        XCTAssertFalse(
            SafeDisplay.coversHousing(content: (2560, 1664), panel: panel, safe: panel))
    }

    func testMacWithoutAHousingYieldsTheNativeModeSoTheRowDedups() {
        // Every Mac without a notch, and any external display: zero inset ⇒ the panel itself, which
        // `macDisplayModes` dedups away instead of offering the same size twice.
        let m = SafeDisplay.mode(
            nativeWidth: 5120, nativeHeight: 1440, topInsetPoints: 0, scale: 1)
        XCTAssertEqual(m.width, 5120)
        XCTAssertEqual(m.height, 1440)
    }
}
