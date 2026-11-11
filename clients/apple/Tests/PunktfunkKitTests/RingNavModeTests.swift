// RingNav.nextMode and .nextRefresh: the sheet's Resolution and Refresh rows, a row each.

import XCTest

@testable import PunktfunkKit

final class RingNavModeTests: XCTestCase {
    private typealias M = RingNav.Mode

    private let uhd = M(width: 3840, height: 2160, hz: 60)

    private func res(_ w: UInt32, _ h: UInt32, _ hz: UInt32 = 60) -> M { M(width: w, height: h, hz: hz) }

    /// Right walks Native, 1440p, 1080p, 720p and round; Left walks back. The rate stays.
    func testResolutionCyclesNativeThenThePresets() {
        let forward = [uhd, res(2560, 1440), res(1920, 1080), res(1280, 720), uhd]
        for (from, to) in zip(forward, forward.dropFirst()) {
            XCTAssertEqual(RingNav.nextMode(mode: from, native: uhd, dir: 1), to)
            XCTAssertEqual(RingNav.nextMode(mode: to, native: uhd, dir: -1), from)
        }
        XCTAssertEqual(RingNav.nextMode(mode: res(1280, 720, 120), native: uhd, dir: 1), res(3840, 2160, 120))
    }

    /// A preset the size of Native is Native's stop, not a second one: on a 1080p display Right
    /// walks Native, 1440p, 720p, and every stop is reached either way.
    func testAPresetEqualToNativeIsOneStop() {
        let fhd = res(1920, 1080)
        let forward = [fhd, res(2560, 1440), res(1280, 720), fhd]
        for (from, to) in zip(forward, forward.dropFirst()) {
            XCTAssertEqual(RingNav.nextMode(mode: from, native: fhd, dir: 1), to)
            XCTAssertEqual(RingNav.nextMode(mode: to, native: fhd, dir: -1), from)
        }
    }

    /// A size off the cycle, a resized window's, steps as if from Native.
    func testAnOffCycleSizeStepsFromNative() {
        XCTAssertEqual(RingNav.nextMode(mode: res(1700, 900), native: uhd, dir: 1), res(2560, 1440))
        XCTAssertEqual(RingNav.nextMode(mode: res(1700, 900), native: uhd, dir: -1), res(1280, 720))
    }

    /// R2-2: a session whose mode reads 0×0 (its handle was gone) has no Native; the rows cycle
    /// the presets and never ask the host for a zero mode.
    func testAZeroModeIsNeverNative() {
        let zero = M(width: 0, height: 0, hz: 0)
        XCTAssertEqual(RingNav.nextMode(mode: res(1280, 720), native: zero, dir: 1), res(2560, 1440))
        XCTAssertEqual(RingNav.nextMode(mode: res(2560, 1440), native: zero, dir: -1), res(1280, 720))
        XCTAssertEqual(RingNav.nextRefresh(mode: res(1280, 720, 60), native: zero, dir: 1).hz, 120)
        XCTAssertEqual(RingNav.nextRefresh(mode: res(1280, 720, 120), native: zero, dir: 1).hz, 60)
    }

    /// Refresh walks Native's rate, then 120 and 60 Hz, skipping one equal to Native's; the size
    /// stays.
    func testRefreshCyclesNativeThenThePresets() {
        let n90 = res(3840, 2160, 90)
        XCTAssertEqual(RingNav.nextRefresh(mode: n90, native: n90, dir: 1).hz, 120)
        XCTAssertEqual(RingNav.nextRefresh(mode: res(3840, 2160, 120), native: n90, dir: 1).hz, 60)
        XCTAssertEqual(RingNav.nextRefresh(mode: res(3840, 2160, 60), native: n90, dir: 1).hz, 90)
        XCTAssertEqual(RingNav.nextRefresh(mode: n90, native: n90, dir: -1).hz, 60)
        XCTAssertEqual(RingNav.nextRefresh(mode: uhd, native: uhd, dir: 1).hz, 120)
        XCTAssertEqual(RingNav.nextRefresh(mode: res(3840, 2160, 120), native: uhd, dir: 1).hz, 60)
        XCTAssertEqual(RingNav.nextRefresh(mode: res(1280, 720, 75), native: uhd, dir: 1), res(1280, 720, 120))
    }
}
