// PyroWave quality as the settings row reads it and the dial carries it.

import PunktfunkCore
import XCTest

@testable import PunktfunkKit

final class PyroWaveQualityTests: XCTestCase {
    private let wire = (kind: UInt8(PUNKTFUNK_IFACE_KIND_ETHERNET), mbps: UInt32(2500))

    /// Core prices the caption and words the warning; a fixed mode and render scale price what
    /// the host would build.
    func testRowReadsAsTheRateItNeeds() {
        var s = EffectiveSettings()
        s.hdrEnabled = false
        let native: (width: Int, height: Int, hz: Int) = (3840, 2160, 120)
        let fits = PyroWaveQuality.lines(s, mode: s.streamMode(native: native), link: wire)
        XCTAssertEqual(fits.caption, "3840×2160 at 120 Hz: 1.6 Gbit/s")
        XCTAssertNil(fits.warning)
        XCTAssertEqual(
            PyroWaveQuality.lines(s, mode: s.streamMode(native: native), link: (0, 0)).warning,
            "Needs 1.6 Gbit/s — more than a 1 GbE link carries.")
        s.width = 1920
        s.height = 1080
        s.refreshHz = 60
        s.renderScale = 0.5
        XCTAssertEqual(
            PyroWaveQuality.lines(s, mode: s.streamMode(native: native), link: wire).caption,
            "960×540 at 60 Hz: 50 Mbit/s")
    }

    /// The dial carries hundredths inside the bounds; the slider lands on tenths.
    func testQualityStaysInsideItsBounds() {
        var s = EffectiveSettings()
        XCTAssertEqual(s.pyrowaveBppX100, 160)
        s.pyrowaveBpp = 9
        XCTAssertEqual(s.pyrowaveBppX100, 200)
        XCTAssertEqual(PyroWaveQuality.snapped(0.5 + 0.1 * 11), 1.6)
        XCTAssertEqual(PyroWaveQuality.snapped(0.1), 0.5)
        XCTAssertEqual(PyroWaveQuality.rungs.count, 16)
        XCTAssertEqual(PyroWaveQuality.rateLabel(kbps: 999_600), "1 Gbit/s")
        XCTAssertEqual(PyroWaveQuality.rateLabel(kbps: 940_000), "940 Mbit/s")
    }

    /// A preset carries the quality under the Rust overlay's key, and it wins over the global.
    func testPresetOverridesTheQuality() throws {
        let o = try JSONDecoder().decode(
            SettingsOverlay.self, from: Data(#"{"pyrowave_bpp":1.2}"#.utf8))
        XCTAssertEqual(o.pyrowaveBpp, 1.2)
        XCTAssertEqual(EffectiveSettings().applying(o).pyrowaveBppX100, 120)
        var cleared = o
        XCTAssertTrue(OverlayField.clear("pyrowave_bpp", in: &cleared))
        XCTAssertTrue(cleared.isEmpty)
    }
}
