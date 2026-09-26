// Locks the DualSense raw-HID rumble report layout to the SDL / Linux hid-playstation spec.
// The motors can only be confirmed on a physical pad, but these guard against a silent byte
// error in the offsets, enable flags, lengths, and the Bluetooth CRC32 — the parts most likely
// to regress unnoticed. macOS-only (DualSenseHID isn't compiled elsewhere).

#if os(macOS)
import XCTest

@testable import PunktfunkKit

final class DualSenseHIDTests: XCTestCase {
    func testUSBReportLayout() {
        let r = DualSenseHID.usbReport(low: 0xAA, high: 0xBB)
        XCTAssertEqual(r.count, 48)
        XCTAssertEqual(r[0], 0x02) // report id
        XCTAssertEqual(r[1], 0x03) // flag0: COMPATIBLE_VIBRATION | HAPTICS_SELECT
        XCTAssertEqual(r[2], 0x00) // flag1 (untouched — leaves lightbar/LEDs alone)
        XCTAssertEqual(r[3], 0xBB) // motor_right = high
        XCTAssertEqual(r[4], 0xAA) // motor_left  = low
        XCTAssertEqual(r[39], 0x04) // flag2: COMPATIBLE_VIBRATION2 (payload offset 38 + report id)
    }

    func testBluetoothReportLayoutAndCRC() {
        let r = DualSenseHID.bluetoothReport(low: 0xAA, high: 0xBB)
        XCTAssertEqual(r.count, 78)
        XCTAssertEqual(r[0], 0x31) // report id
        XCTAssertEqual(r[1], 0x00) // seq/tag
        XCTAssertEqual(r[2], 0x10) // magic
        XCTAssertEqual(r[3], 0x03) // flag0
        XCTAssertEqual(r[5], 0xBB) // motor_right = high (payload offset 2 + 3-byte BT header)
        XCTAssertEqual(r[6], 0xAA) // motor_left  = low
        XCTAssertEqual(r[41], 0x04) // flag2 (payload offset 38 + 3)

        // Trailing CRC32 = standard CRC32 over (0xA2 seed + report[0..<74]), little-endian.
        let expected = DualSenseHID.crc32(seed: 0xA2, r[0..<74])
        let stored = UInt32(r[74]) | (UInt32(r[75]) << 8) | (UInt32(r[76]) << 16) | (UInt32(r[77]) << 24)
        XCTAssertEqual(stored, expected)
    }

    /// flag1 bit 0 enables `mute_button_led` (payload offset 8); nothing else is claimed.
    func testMicLEDReportLayout() {
        let u = DualSenseHID.usbMicReport(mode: 2)
        XCTAssertEqual(u.count, 48)
        XCTAssertEqual(u[0], 0x02)
        XCTAssertEqual(u[2], 0x01) // flag1: mic-mute LED
        XCTAssertEqual(u[9], 0x02) // pulse
        XCTAssertEqual(u.filter { $0 != 0 }.count, 3)

        let b = DualSenseHID.bluetoothMicReport(mode: 1)
        XCTAssertEqual(b[0], 0x31)
        XCTAssertEqual(b[4], 0x01) // flag1 (payload offset 1 + 3)
        XCTAssertEqual(b[11], 0x01) // mode (payload offset 8 + 3)
        let stored = UInt32(b[74]) | (UInt32(b[75]) << 8) | (UInt32(b[76]) << 16) | (UInt32(b[77]) << 24)
        XCTAssertEqual(stored, DualSenseHID.crc32(seed: 0xA2, b[0..<74]))
    }

    func testCRC32MatchesStandardCheckVector() {
        // The canonical CRC32 check value: CRC32("123456789") == 0xCBF43926. Our helper folds a
        // seed byte in first, so feed seed='1' and the rest — proving poly/reflection/init/final.
        let crc = DualSenseHID.crc32(seed: UInt8(ascii: "1"), Array("23456789".utf8))
        XCTAssertEqual(crc, 0xCBF4_3926)
    }

    // MARK: - Device selection (B14)

    /// With two DualSenses attached, each renderer must drive its OWN device. The old code took
    /// `Set.first` from an unordered set, so the pad→device binding was a coin flip that could
    /// point both renderers at the same pad.
    func testPreferredIndexHonoursAnExplicitLocation() {
        let ids: [UInt32?] = [0x1D18_0000, 0x1420_0000, 0x1411_0000]
        XCTAssertEqual(DualSenseHID.preferredIndex(among: ids, preferring: 0x1420_0000), 1)
        XCTAssertEqual(DualSenseHID.preferredIndex(among: ids, preferring: 0x1D18_0000), 0)
    }

    /// No preference (or one the pad no longer has): fall back to the LOWEST id — arbitrary, but
    /// stable across calls, which `Set.first` was not.
    func testPreferredIndexFallsBackToTheLowestIdDeterministically() {
        let ids: [UInt32?] = [0x1D18_0000, 0x1420_0000, 0x1411_0000]
        XCTAssertEqual(DualSenseHID.preferredIndex(among: ids, preferring: nil), 2)
        // A wanted id that is gone (pad unplugged between enumeration and open) must not fail the
        // open — it degrades to the same stable fallback.
        XCTAssertEqual(DualSenseHID.preferredIndex(among: ids, preferring: 0xDEAD_BEEF), 2)
    }

    /// A device IOKit reports no location for must never displace one it can place.
    func testPreferredIndexSortsUnplaceableDevicesLast() {
        XCTAssertEqual(DualSenseHID.preferredIndex(among: [nil, 0x1420_0000], preferring: nil), 1)
        XCTAssertEqual(DualSenseHID.preferredIndex(among: [nil, nil], preferring: nil), 0)
        XCTAssertNil(DualSenseHID.preferredIndex(among: [], preferring: nil))
    }
}
#endif
