#if os(macOS)
import AppKit
import XCTest
@testable import PunktfunkKit

final class ScreenPlanTests: XCTestCase {
    /// A display beside a HiDPI main one sits in the main picture's pixels, y counted down from
    /// the main display's top although AppKit counts up from the bottom.
    func testADisplaySitsInTheFirstPicturesPixels() {
        let first = CGRect(x: 0, y: 0, width: 1512, height: 982)
        let mode: (width: UInt32, height: UInt32) = (3024, 1964)
        let right = NSScreen.offset(
            of: CGRect(x: 1512, y: 0, width: 1920, height: 1080), size: (3840, 2160),
            from: first, mode: mode)
        XCTAssertEqual(right.x, 3024)
        XCTAssertEqual(right.y, -196, "its top stands 98 points above the main display's")
        let above = NSScreen.offset(
            of: CGRect(x: -400, y: 982, width: 1920, height: 1080), size: (3840, 2160),
            from: first, mode: mode)
        XCTAssertEqual(above.x, -800)
        XCTAssertEqual(above.y, -2160)
    }

    /// A 1x display left of or above a 2x main one touches the main picture's edge.
    func testASparserDisplayLeftOrAboveLeavesNoGap() {
        let first = CGRect(x: 0, y: 0, width: 1512, height: 982)
        let mode: (width: UInt32, height: UInt32) = (3024, 1964)
        let left = NSScreen.offset(
            of: CGRect(x: -1920, y: 0, width: 1920, height: 1080), size: (1920, 1080),
            from: first, mode: mode)
        XCTAssertEqual(left.x, -1920, "not -3840")
        XCTAssertEqual(left.y, -196)
        let above = NSScreen.offset(
            of: CGRect(x: 0, y: 982, width: 1920, height: 1080), size: (1920, 1080),
            from: first, mode: mode)
        XCTAssertEqual(above.x, 0)
        XCTAssertEqual(above.y, -1080, "not -2160")
    }

    /// A pointer lands on the pixel the fit draws there, and the bars land nowhere.
    func testAPointerLandsOnThePicture() {
        // A square picture in a 400×200 px view: half scale, 100 px bars left and right.
        let bounds = CGRect(x: 0, y: 0, width: 200, height: 100)
        let middle = StreamLayerView.picturePoint(
            CGPoint(x: 100, y: 50), bounds: bounds, scale: 2, content: (400, 400), fit: .fit)
        XCTAssertEqual(middle?.x, 200)
        XCTAssertEqual(middle?.y, 200)
        let top = StreamLayerView.picturePoint(
            CGPoint(x: 100, y: 99), bounds: bounds, scale: 2, content: (400, 400), fit: .fit)
        XCTAssertEqual(top?.y, 4, "view y counts up, picture y counts down")
        XCTAssertNil(StreamLayerView.picturePoint(
            CGPoint(x: 5, y: 50), bounds: bounds, scale: 2, content: (400, 400), fit: .fit))
        XCTAssertNil(StreamLayerView.picturePoint(
            CGPoint(x: 100, y: 50), bounds: bounds, scale: 2, content: (0, 0), fit: .fit))
    }

    /// A picture filling a 27-inch 4K panel (597×336 mm) is that size at any mode. A window
    /// half the panel is half of it, Match window is the whole panel whatever the window, and a
    /// display with no real size reads unknown.
    func testThePictureSizeIsTheGlassItCovers() {
        func size(
            _ mode: (width: UInt32, height: UInt32), over covered: (width: Int, height: Int),
            mm: CGSize = CGSize(width: 597, height: 336), points: CGSize = CGSize(width: 2560, height: 1440),
            matchWindow: Bool = false
        ) -> [UInt16] {
            let s = NSScreen.pictureSizeMM(
                mode: mode, covered: covered, fit: .fit, panel: (3840, 2160), mm: mm, points: points,
                matchWindow: matchWindow)
            return [s.width, s.height]
        }
        XCTAssertEqual(size((3840, 2160), over: (3840, 2160)), [597, 336])
        XCTAssertEqual(size((1920, 1080), over: (3840, 2160)), [597, 336], "render scale 0.5")
        XCTAssertEqual(size((3840, 2160), over: (1920, 1080)), [299, 168], "a window half the panel")
        XCTAssertEqual(
            size((1280, 720), over: (1280, 720), matchWindow: true), [597, 336],
            "the host keeps one size while the mode follows the window")
        XCTAssertEqual(size((3840, 2160), over: (3840, 2160), mm: .zero), [0, 0])
        XCTAssertEqual(
            size((3840, 2160), over: (3840, 2160), mm: CGSize(width: 60, height: 34)), [0, 0],
            "centimetres read as millimetres: over 1000 PPI")
        // macOS's guess for a display without EDID: 72 DPI of its points.
        let guess = CGSize(width: 1920 * 25.4 / 72, height: 1080 * 25.4 / 72)
        XCTAssertEqual(
            size((3840, 2160), over: (3840, 2160), mm: guess, points: CGSize(width: 1920, height: 1080)),
            [0, 0])
        // Many TVs write their aspect ratio, 16×9 cm, where the size belongs.
        for matchWindow in [false, true] {
            XCTAssertEqual(
                size(
                    (1920, 1080), over: (1920, 1080), mm: CGSize(width: 160, height: 90),
                    matchWindow: matchWindow),
                [0, 0])
        }
    }
}
#endif
