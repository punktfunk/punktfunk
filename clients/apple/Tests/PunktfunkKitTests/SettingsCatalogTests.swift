import XCTest

@testable import PunktfunkKit

/// `clients/shared/settings-catalog.json` against the console document this app writes: every
/// shared setting either rides the document under its catalogue key or is one this client does
/// not have. A new catalogue key fails here until one of the two is true.
final class SettingsCatalogTests: XCTestCase {
    /// Catalogue keys with no Apple store. The console gates their rows off on this platform.
    private static let absent: Set<String> = [
        "follow_os_theme", "reduce_motion", "reduce_ui_resolution", "low_latency", "decoder",
        "audio_route", "cursor_gestures", "forward_pad", "pad_haptics", "pad_speaker",
        "ds_capture", "second_screen",
    ]

    /// Where a catalogue key sits in the document when it is not a key of its own.
    private static let stored: [String: String] = [
        "resolution": "width",
        "rumble_on_phone": "android.rumble_on_phone",
        "gyro_on_phone": "android.gyro_on_phone",
        "sc2_capture": "android.sc2_capture",
    ]

    func testEveryCatalogueKeyIsStoredOrAbsent() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/settings-catalog.json")
        let doc = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let keys = try XCTUnwrap(doc["settings"] as? [[String: Any]]).compactMap {
            $0["key"] as? String
        }
        XCTAssertFalse(keys.isEmpty)

        let defaults = try XCTUnwrap(UserDefaults(suiteName: "settings-catalog-tests"))
        defaults.removePersistentDomain(forName: "settings-catalog-tests")
        let written = ConsoleSettings.document(defaults)
        for key in keys where !Self.absent.contains(key) {
            let name = Self.stored[key] ?? key
            XCTAssertNotNil(written[name], "\(key) is in the catalogue but not in the document")
        }
        for key in Self.absent {
            XCTAssertTrue(keys.contains(key), "\(key) is listed absent but not in the catalogue")
        }
    }
}
