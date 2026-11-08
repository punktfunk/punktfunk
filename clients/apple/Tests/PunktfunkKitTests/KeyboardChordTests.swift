import Foundation
import XCTest

@testable import PunktfunkKit

/// The keyboard shortcuts' inputs: the iPad's GameController key stream reaches the engine as
/// VKs through `hidToVK`, with either side of a modifier folded onto one key.
final class KeyboardChordTests: XCTestCase {
    // HID usages: left Control/Shift/Option 0xE0/0xE1/0xE2, right 0xE4/0xE5/0xE6, Command 0xE3/0xE7.
    func testEitherSideOfAModifierIsOneShortcutKey() {
        let fold = { (usage: Int) in InputCapture.hidToVK[usage].map(InputCapture.shortcutKey) }
        XCTAssertEqual(fold(0xE0), keyVk("ctrl"))
        XCTAssertEqual(fold(0xE4), keyVk("ctrl"))
        XCTAssertEqual(fold(0xE2), keyVk("alt"))
        XCTAssertEqual(fold(0xE6), keyVk("alt"))
        XCTAssertEqual(fold(0xE1), keyVk("shift"))
        XCTAssertEqual(fold(0xE5), keyVk("shift"))
        XCTAssertEqual(fold(0xE3), fold(0xE7), "either ⌘ is one key")
    }

    /// The default letters are the VKs their keys send: O opens the dial, Q releases, A mutes,
    /// D ends the stream, S cycles the statistics, C flips clipboard sharing.
    func testDefaultLettersAreTheVKsTheirKeysSend() {
        for (usage, name) in [(0x06, "c"), (0x12, "o"), (0x14, "q"), (0x04, "a"), (0x07, "d"), (0x16, "s")] {
            XCTAssertEqual(InputCapture.hidToVK[usage], keyVk(name), name)
        }
    }

    /// ⌃⌥⇧O down through the folded keys fires the dial and lifts the modifiers the host holds.
    func testTheDefaultDialChordFiresOnItsKey() throws {
        var engine = Shortcuts.Engine(Shortcuts.defaults(client: .apple).bindings, device: .keyboard)
        let now = ContinuousClock.now
        var outs: [Shortcuts.Out] = []
        for usage in [0xE0, 0xE6, 0xE1, 0x12] {
            let vk = try XCTUnwrap(InputCapture.hidToVK[usage])
            outs += engine.step(.press(InputCapture.shortcutKey(vk)), at: now)
        }
        XCTAssertEqual(outs.last, .fire(.ring, .once))
        XCTAssertTrue(outs.contains(.lift([0x11, 0x12, 0x10])))
    }
}
