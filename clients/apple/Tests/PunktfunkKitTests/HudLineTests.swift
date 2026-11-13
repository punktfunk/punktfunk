// The core formats the stats overlay; these pin the Swift half: the line decode, the decoded size
// beside a larger host mode, and a live tier cycle that names its session and never moves the
// stored default.

import PunktfunkShared
import XCTest

@testable import PunktfunkKit

final class HudLineTests: XCTestCase {
    func testDecodesRoleTaggedLines() {
        let lines = PunktfunkConnection.HudLine.decode(
            "0\t120 fps · 24.3 Mb/s\n3\tlost 3 (2.4%)\nno tab here\n9\tunknown role\n")
        XCTAssertEqual(lines.map(\.text), ["120 fps · 24.3 Mb/s", "lost 3 (2.4%)", "unknown role"])
        XCTAssertEqual(lines.map(\.role), [.primary, .warn, .primary])
        XCTAssertEqual(PunktfunkConnection.HudLine.decode(""), [])
    }

    /// A picture at the host's mode reads as the mode alone; one the host scaled down leads with
    /// its own size and names the mode after it. The desktop twin is
    /// `the_mode_line_names_the_host_size_beside_a_smaller_picture`.
    func testModeLineNamesTheHostSizeBesideASmallerPicture() {
        let lines = [
            PunktfunkConnection.HudLine(role: .primary, text: "3840×2160@60 · 120 fps · 24.3 Mb/s"),
            PunktfunkConnection.HudLine(role: .muted, text: "120 fps · 24.3 Mb/s"),
        ]
        let mode: (width: UInt32, height: UInt32, refreshHz: UInt32) = (3_840, 2_160, 60)
        let name = { (picture: (width: UInt32, height: UInt32)?) in
            PunktfunkConnection.HudLine.naming(picture: picture, mode: mode, in: lines)
        }
        XCTAssertEqual(name(nil), lines)
        XCTAssertEqual(name((3_840, 2_160)), lines)
        XCTAssertEqual(name((3_840, 2_176)), lines, "padded output")
        let host = L10n.overlayHostRenders(width: "3840", height: "2160")
        XCTAssertEqual(name((1_920, 1_080)).map(\.text), [
            "1920×1080@60 · \(host) · 120 fps · 24.3 Mb/s", "120 fps · 24.3 Mb/s",
        ])
        XCTAssertEqual(name((1_920, 1_080))[0].role, .primary)
    }

    func testCycleRequestNamesItsSession() {
        let key = DefaultsKey.statsVerbosity
        let saved = UserDefaults.standard.string(forKey: key)
        defer {
            if let saved {
                UserDefaults.standard.set(saved, forKey: key)
            } else {
                UserDefaults.standard.removeObject(forKey: key)
            }
        }
        UserDefaults.standard.set("detailed", forKey: key)
        let session = NSObject()
        let posted = expectation(forNotification: .punktfunkStatsCycled, object: session)
        StatsVerbosity.requestCycle(for: session)
        wait(for: [posted], timeout: 1)
        XCTAssertEqual(UserDefaults.standard.string(forKey: key), "detailed", "the stored tier moved")
    }
}
