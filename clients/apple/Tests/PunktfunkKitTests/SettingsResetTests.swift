import XCTest

@testable import PunktfunkKit

/// Reset reads back as a fresh install through the console document, which carries every key the
/// app owns, and leaves only records behind: hosts, presets and the default host.
final class SettingsResetTests: XCTestCase {
    func testResetReadsAsAFreshInstallAndKeepsHostRecords() throws {
        let suite = "settings-reset-tests"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defaults.removePersistentDomain(forName: suite)
        defer { defaults.removePersistentDomain(forName: suite) }
        let fresh = ConsoleSettings.document(defaults)
        XCTAssertNotNil(fresh["shortcuts"], "the shortcuts blob rides the document, so it is moved and reset below")

        // Every key the document carries, moved off its default and stored. The null default
        // host is no property-list value; it is set by hand below.
        var moved = fresh
        moved["default_host"] = nil
        for (key, value) in fresh {
            switch value {
            case let b as Bool: moved[key] = !b
            case let n as Int: moved[key] = n + 1
            case let d as Double: moved[key] = d + 0.25
            case let s as String: moved[key] = s + "x"
            default: break
            }
        }
        ConsoleSettings.apply(moved, defaults)
        defaults.set(Data([1, 2, 3]), forKey: DefaultsKey.hosts)
        defaults.set(Data([4, 5]), forKey: DefaultsKey.presets)
        defaults.set("desk", forKey: DefaultsKey.defaultHost)
        let differs = { (a: [String: Any], b: [String: Any]) -> Bool in
            var a = a, b = b
            a["default_host"] = nil
            b["default_host"] = nil
            return !NSDictionary(dictionary: a).isEqual(to: b)
        }
        XCTAssertTrue(differs(ConsoleSettings.document(defaults), fresh), "the settings moved")

        SettingDefault.resetAll(defaults)

        XCTAssertFalse(differs(ConsoleSettings.document(defaults), fresh), "a fresh install's")
        XCTAssertEqual(defaults.data(forKey: DefaultsKey.hosts), Data([1, 2, 3]))
        XCTAssertEqual(defaults.data(forKey: DefaultsKey.presets), Data([4, 5]))
        XCTAssertEqual(defaults.string(forKey: DefaultsKey.defaultHost), "desk")
        let left = Set((defaults.persistentDomain(forName: suite) ?? [:]).keys)
        XCTAssertEqual(
            left,
            [DefaultsKey.hosts, DefaultsKey.presets, DefaultsKey.defaultHost,
             ConsoleSettings.documentKey],
            "only records survive")
    }
}
