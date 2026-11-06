import XCTest

@testable import PunktfunkKit

/// `clients/shared/settings-catalog.json` against this client: every entry naming `apple` rides
/// the console document under its key and is a row of the settings screen (or one the console
/// draws), in its category and tier, worded by its `player_<key>` message, with its options, its
/// default and its preset tier. A row for a setting the catalogue does not give to Apple fails
/// here too.
///
/// The settings screen is SwiftUI code in the app target, not a table, so its rows are read from
/// the source: which `L10n.player<Key>` words a row, in which section, inside which
/// `advancedSection` closure.
final class SettingsCatalogTests: XCTestCase {
    /// Entries naming `apple` that the settings screen has no row for: the console draws them.
    private static let consoleOnly: [String: String] = [
        "library_sections": "Customize on the Library header",
        "host_sort": "Sort on the Hosts header",
        "host_grouping": "Group on the Hosts header",
    ]

    /// Where a catalogue key sits in the document when it is not a key of its own.
    private static let stored: [String: String] = [
        "resolution": "width",
        "rumble_on_phone": "android.rumble_on_phone",
        "gyro_on_phone": "android.gyro_on_phone",
        "sc2_capture": "android.sc2_capture",
        "spatial_audio": "visionos.spatial_audio",
        "screen": "visionos.screen",
    ]

    /// The document carries it for the console, though the catalogue leaves `apple` out: the OS
    /// answers it, and the document holds the answer.
    private static let documentOnly: Set<String> = ["reduce_motion"]

    /// Entries naming `apple` that the document does not carry: the app keeps its own id for the
    /// controller it forwards (`gamepadID`, a different grammar from the desktop's).
    private static let notInDocument: Set<String> = ["forward_pad"]

    /// Rows that sit in another category on this screen than in the catalogue.
    private static let otherCategory: [String: String] = [
        "ui_palette": "controllers",  // an Apple TV reaches it only from the gamepad settings
        "gamepad_ui_enabled": "controllers",
        "gamepad_ui_mode": "controllers",
    ]

    /// Pickers that list the catalogue's options in another order, or fewer, with the list.
    private static let differs: [String: [String]] = [
        "codec": ["auto", "hevc", "av1", "h264", "pyrowave"],
        "gamepad": ["auto", "xbox360", "xboxone", "dualsense", "dualshock4", "steamdeck"],
    ]

    /// The sections of the settings screen, by the category each draws.
    private static let roots: [String: String] = [
        "sessionSection": "general", "overlaySection": "general", "librarySection": "general",
        "showAdvancedSection": "general", "generalAdvancedSection": "general",
        "resolutionSection": "display", "pictureSection": "display",
        "displayAdvancedSection": "display",
        "pointerSection": "input", "inputSection": "input",
        "audioSection": "audio", "controllersSection": "controllers",
    ]

    // MARK: - Reading the files

    private static var apple: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // PunktfunkKitTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // apple
    }

    /// A source file without its comment lines, so a name in prose is not a use.
    private static func read(_ path: String) throws -> String {
        try String(contentsOf: apple.appendingPathComponent(path), encoding: .utf8)
            .split(separator: "\n", omittingEmptySubsequences: false)
            .filter { !$0.drop(while: { $0 == " " }).hasPrefix("//") }
            .joined(separator: "\n")
    }

    private static func catalogue() throws -> [[String: Any]] {
        let url = apple.deletingLastPathComponent().appendingPathComponent(
            "shared/settings-catalog.json")
        let doc = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        return try XCTUnwrap(doc["settings"] as? [[String: Any]])
    }

    private static func text(_ v: Any?) -> String {
        switch v {
        case let b as Bool: return String(b)
        case let i as Int: return String(i)
        case let d as Double: return d == d.rounded() ? String(Int(d)) : String(d)
        case let s as String: return s
        case let n as NSNumber: return n.boolValue ? "true" : "false"
        default: return "\(String(describing: v))"
        }
    }

    private static func on(_ e: [String: Any], _ platform: String) -> Bool {
        (e["on"] as? [String] ?? []).contains(platform)
    }

    /// `player_stats_scale_pct` as the generated accessor spells it: `playerStatsScalePct`.
    private static func accessor(_ key: String) -> String {
        "player"
            + key.split(separator: "_").map { $0.prefix(1).uppercased() + $0.dropFirst() }
            .joined()
    }

    private func matches(_ pattern: String, in s: String) -> [NSTextCheckingResult] {
        let re = try! NSRegularExpression(pattern: pattern)
        return re.matches(in: s, range: NSRange(s.startIndex..., in: s))
    }

    private func groups(_ pattern: String, in s: String) -> [[String]] {
        matches(pattern, in: s).map { m in
            (1..<m.numberOfRanges).map { i in
                Range(m.range(at: i), in: s).map { String(s[$0]) } ?? ""
            }
        }
    }

    // MARK: - Store, preset tier, defaults

    func testEveryCatalogueEntryRidesTheDocument() throws {
        let entries = try Self.catalogue()
        XCTAssertFalse(entries.isEmpty)
        let defaults = try XCTUnwrap(UserDefaults(suiteName: "settings-catalog-tests"))
        defaults.removePersistentDomain(forName: "settings-catalog-tests")
        let written = ConsoleSettings.document(defaults)
        for e in entries {
            let key = e["key"] as! String
            let name = Self.stored[key] ?? key
            if Self.on(e, "apple") {
                if !Self.notInDocument.contains(key) {
                    XCTAssertNotNil(written[name], "\(key) is in the catalogue but not in the document")
                }
            } else if !Self.documentOnly.contains(key) {
                XCTAssertNil(written[name], "\(key) is in the document, and the catalogue leaves apple out")
            }
        }
        for key in Self.documentOnly {
            XCTAssertTrue(entries.contains { $0["key"] as? String == key }, "\(key)")
        }
    }

    func testDefaultsMatchAFreshInstall() throws {
        let defaults = try XCTUnwrap(UserDefaults(suiteName: "settings-catalog-tests"))
        defaults.removePersistentDomain(forName: "settings-catalog-tests")
        let written = ConsoleSettings.document(defaults)
        for e in try Self.catalogue() where Self.on(e, "apple") {
            guard let want = e["default"] else { continue }
            let key = e["key"] as! String
            guard !Self.notInDocument.contains(key) else { continue }
            XCTAssertEqual(Self.text(written[Self.stored[key] ?? key]), Self.text(want), "\(key): default")
        }
    }

    func testPresetTierMatchesTheOverlay() throws {
        let scope = try Self.read("Sources/PunktfunkClient/Settings/SettingsView+Scope.swift")
        var presettable = Set(groups("\\.init\\(name: \"([a-z0-9_]+)\"", in: scope).map { $0[0] })
        // Width, height and Match window share the alias.
        if scope.contains("OverlayField.resolution") { presettable.insert("resolution") }
        for e in try Self.catalogue() where Self.on(e, "apple") {
            let key = e["key"] as! String
            // The overlay also carries the Mac's three-way fullscreen and the quick-action ring.
            let preset = presettable.contains(key)
            XCTAssertEqual(preset, e["preset"] as? Bool, "\(key): preset")
        }
    }

    // MARK: - Options

    func testOptionsMatchTheCatalogue() throws {
        let options = try Self.read("Sources/PunktfunkClient/Settings/SettingsOptions.swift")
        let sections = try Self.read("Sources/PunktfunkClient/Settings/SettingsView+Sections.swift")
        let hud = try Self.read("Sources/PunktfunkClient/Session/HUDPlacement.swift")
        var failures: [String] = []
        for e in try Self.catalogue() where Self.on(e, "apple") {
            let key = e["key"] as! String
            if Self.consoleOnly[key] != nil { continue }
            let got = try listed(key, options: options, sections: sections, hud: hud)
            let want = e["options"] as? [String]
            switch (got, want) {
            case (nil, nil): continue
            case (let got?, let want?):
                if let listed = Self.differs[key] {
                    if got != listed || listed == want || !Set(listed).isSubset(of: Set(want)) {
                        failures.append("\(key): \(got), expected the open row \(listed) of \(want)")
                    }
                } else if got != want {
                    failures.append("\(key): \(got), the catalogue's \(want)")
                }
            default:
                failures.append("\(key): options \(String(describing: got)), the catalogue's \(String(describing: want))")
            }
        }
        XCTAssertEqual(failures, [])
    }

    /// The stored value of each entry of a picker's list, in the order the screen offers them.
    /// `nil` for a setting that is not a list. Only what a hardware probe adds is left out.
    private func listed(_ key: String, options: String, sections: String, hud: String) throws
        -> [String]?
    {
        /// The text of `static let <name>` up to the next declaration.
        func table(_ name: String) throws -> String {
            let at = try XCTUnwrap(options.range(of: "static let \(name)"), name)
            let rest = options[at.lowerBound...].dropFirst(10)
            let end = rest.range(of: "\n    static ")?.lowerBound ?? rest.endIndex
            return String(rest[..<end])
        }
        func tags(_ name: String, _ pattern: String = ", ?(\"[A-Za-z0-9]+\"|\\d+)\\)") throws -> [String] {
            groups(pattern, in: try table(name)).map { $0[0].replacingOccurrences(of: "\"", with: "") }
        }
        switch key {
        case "start_in": return StartIn.allCases.map(\.rawValue)
        case "stats_verbosity": return StatsVerbosity.allCases.map(\.rawValue)
        case "gamepad_ui_mode":
            let named = groups("GamepadUIEnvironment\\.(mode[A-Za-z]+)", in: try table("gamepadUIModes"))
                .map { $0[0] }
            return named.map {
                $0 == "modeWhenConnected"
                    ? GamepadUIEnvironment.modeWhenConnected : GamepadUIEnvironment.modeAlways
            }
        case "hud_placement":
            let line = try XCTUnwrap(groups("case ((?:[a-zA-Z]+(?:, )?)+)\n", in: hud).first)[0]
            return line.components(separatedBy: ", ")
        case "stats_scale_pct":
            let list = try XCTUnwrap(groups("= *\n? *\\[([0-9, ]+)\\]\\.map", in: try table("statsScales")).first)[0]
            return list.components(separatedBy: ", ")
        case "background_timeout_minutes":
            let list = try XCTUnwrap(groups("options: \\[([0-9, ]+)\\]\\.map", in: sections).first)[0]
            return list.components(separatedBy: ", ")
        case "refresh_hz":
            // A display's own rates: the Mac's ladder is the longest, and the catalogue lists it.
            let ladders = groups("let ladder = \\[([0-9, ]+)\\]", in: options).map {
                ["0"] + $0[0].components(separatedBy: ", ")
            }
            return ladders.max { $0.count < $1.count }
        case "video_fit": return VideoFit.allCases.map(\.rawValue)
        case "present_priority": return try tags("presentPriorities")
        case "smooth_buffer":
            let fn = try XCTUnwrap(options.range(of: "func smoothBuffers"))
            let block = try XCTUnwrap(options.range(of: "return [", range: fn.upperBound..<options.endIndex))
            let rest = String(options[block.upperBound...])
            return groups(", (\\d)\\)", in: String(rest.prefix(while: { $0 != "]" }))).map { $0[0] }
        case "render_scale": return RenderScale.presets.map { Self.text($0) }
        case "codec":
            // The base list, AV1 inserted at its index, PyroWave appended: as `codecs(current:)` builds it.
            let body = try XCTUnwrap(groups("var options: \\[[^\\]]*\\] = \\[([^\\]]*)\\]", in: options).first)[0]
            var list = groups("\"([a-z0-9]+)\"\\)", in: body).map { $0[0] }
            let insert = try XCTUnwrap(groups("insert\\(\\(\"AV1\", \"(av1)\"\\), at: (\\d)\\)", in: options).first)
            list.insert(insert[0], at: Int(insert[1])!)
            XCTAssertTrue(options.contains("options.append((String(localized: L10n.playerCodecPyrowave), \"pyrowave\"))"))
            return list + ["pyrowave"]
        case "compositor":
            return try tags("compositors").map { ConsoleSettings.compositorName(Int($0)!) }
        case "audio_channels": return try tags("audioChannels")
        case "audio_format":
            return groups("AudioFormatChoice\\.([a-z0-9]+)\\.rawValue", in: try table("audioFormats"))
                .map { $0[0] }
        case "touch_mode":
            return picker(sections, "Picker(L10n.playerTouchMode", "TouchInputMode")
        case "mouse_mode":
            return picker(sections, "Picker(L10n.playerMouseMode", "MouseInputMode")
        case "screens":
            let at = try XCTUnwrap(sections.range(of: "L10n.playerScreens,"))
            let rest = String(sections[at.upperBound...])
            return groups("tag: \"([a-z]+)\"", in: String(rest.prefix(400))).map { $0[0] }
        case "screen": return ScreenCurve.allCases.map(\.rawValue)
        case "host_sort": return HostSort.allCases.map(\.rawValue)
        case "host_grouping": return HostGrouping.allCases.map(\.rawValue)
        case "library_view": return LibraryArrangement.all.map(\.stored)
        case "forward_pad": return nil
        case "gamepad":
            return try tags("padTypes").map { ConsoleSettings.padTypeName(Int($0)!) }
        case "system_buttons": return try tags("systemButtons")
        case "guide_gesture": return try tags("guideGestures")
        default: return nil
        }
    }

    /// The `<Enum>.<case>.rawValue` entries of the picker that opens at `start`, in order.
    private func picker(_ source: String, _ start: String, _ type: String) -> [String] {
        guard let at = source.range(of: start) else { return [] }
        let body = String(source[at.upperBound...].prefix(600))
        return groups("\(type)\\.([a-z]+)\\.rawValue", in: body).map { $0[0] }
    }

    // MARK: - Rows

    /// Each `var`/`func` member of the settings extension, with where its text starts.
    private func declarations(_ source: String) -> [(name: String, start: Int, end: Int)] {
        let found = matches(
            "(?m)^    (?:@ViewBuilder )?(?:private )?(?:static )?(?:var|func) ([A-Za-z0-9_]+)",
            in: source)
        let ns = source as NSString
        return found.enumerated().map { i, m in
            (
                ns.substring(with: m.range(at: 1)), m.range.location,
                i + 1 < found.count ? found[i + 1].range.location : ns.length
            )
        }
    }

    /// The `{ ... }` closure after each `advancedSection(` call.
    private func advancedClosures(_ source: String) -> [Range<Int>] {
        let chars = Array(source.utf16)
        func match(_ open: Int, _ a: UInt16, _ b: UInt16) -> Int {
            var depth = 0
            for i in open..<chars.count {
                if chars[i] == a { depth += 1 }
                if chars[i] == b {
                    depth -= 1
                    if depth == 0 { return i }
                }
            }
            return chars.count
        }
        return matches("advancedSection\\(", in: source).map { m in
            let paren = m.range.location + m.range.length - 1
            let close = match(paren, 0x28, 0x29)
            var open = close
            while open < chars.count, chars[open] != 0x7B { open += 1 }
            return open..<match(open, 0x7B, 0x7D)
        }
    }

    func testRowsSitInTheirCategoryAndTier() throws {
        let source = try Self.read("Sources/PunktfunkClient/Settings/SettingsView+Sections.swift")
        let ns = source as NSString
        let decls = declarations(source)
        let closures = advancedClosures(source)
        func body(_ d: (name: String, start: Int, end: Int)) -> String {
            ns.substring(with: NSRange(location: d.start, length: d.end - d.start))
        }
        func mentions(_ text: String, _ name: String) -> Bool {
            !matches("\\b\(name)\\b", in: text).isEmpty
        }

        // A helper sits in the category of the section that calls it, and is advanced when an
        // advanced closure calls it.
        var category: [String: String] = Self.roots
        var queue = Array(Self.roots.keys)
        while let name = queue.popLast() {
            guard let d = decls.first(where: { $0.name == name }) else {
                XCTFail("no section \(name)")
                continue
            }
            for other in decls where category[other.name] == nil && mentions(body(d), other.name) {
                category[other.name] = category[name]
                queue.append(other.name)
            }
        }
        let closureText = closures.map { ns.substring(with: NSRange(location: $0.lowerBound, length: $0.count)) }
        var advanced = Set<String>()
        var pending = decls.map(\.name).filter { n in closureText.contains { mentions($0, n) } }
        while let name = pending.popLast() {
            guard advanced.insert(name).inserted, let d = decls.first(where: { $0.name == name })
            else { continue }
            pending += decls.map(\.name).filter { $0 != name && mentions(body(d), $0) }
        }

        var failures: [String] = []
        for e in try Self.catalogue() {
            let key = e["key"] as! String
            // V-Sync, a brand, has no message: its row spells it.
            let label = key == "vsync" ? "\"V-Sync\"" : "L10n\\.\(Self.accessor(key))(?![A-Za-z0-9])"
            let uses: [(String, Bool)] = matches(label, in: source)
                .compactMap { m in
                    let at = m.range.location
                    guard let d = decls.last(where: { $0.start <= at }) else { return nil }
                    guard let page = category[d.name] else {
                        failures.append("\(key) is worded in \(d.name), a member this test does not place")
                        return nil
                    }
                    let inClosure = closures.contains { $0.contains(at) }
                    return (page, inClosure || advanced.contains(d.name))
                }
            if !Self.on(e, "apple") {
                if !uses.isEmpty { failures.append("\(key) has a row, and the catalogue leaves apple out") }
                continue
            }
            if Self.consoleOnly[key] != nil {
                if !uses.isEmpty { failures.append("\(key) has a row, and is listed as the console's") }
                continue
            }
            let want = (Self.otherCategory[key] ?? e["category"] as! String, e["advanced"] as! Bool)
            if uses.isEmpty {
                failures.append("\(key): no row on the settings screen")
            } else if let bad = uses.first(where: { $0 != want }) {
                failures.append("\(key): row in \(bad), the catalogue's \(want)")
            }
        }
        XCTAssertEqual(failures, [])
    }
}
