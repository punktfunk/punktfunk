import XCTest

@testable import PunktfunkKit

/// Pins the iPad's ⌃⌥⇧ chord match on the GameController key stream, from HID usages through
/// `hidToVK`, the way `InputCapture`'s key handler reads them.
final class KeyboardChordTests: XCTestCase {
    private func held(_ usages: [Int]) -> Set<UInt32> {
        Set(usages.compactMap { InputCapture.hidToVK[$0] })
    }

    // HID usages: left Control/Shift/Option 0xE0/0xE1/0xE2, right 0xE4/0xE5/0xE6, Command 0xE3.
    func testEitherSideOfEachModifierCounts() {
        XCTAssertTrue(InputCapture.holdsChordModifiers(held([0xE0, 0xE2, 0xE1])))
        XCTAssertTrue(InputCapture.holdsChordModifiers(held([0xE4, 0xE6, 0xE5])))
        XCTAssertTrue(InputCapture.holdsChordModifiers(held([0xE0, 0xE6, 0xE1])))
    }

    func testAMissingModifierIsNoChord() {
        XCTAssertFalse(InputCapture.holdsChordModifiers([]))
        XCTAssertFalse(InputCapture.holdsChordModifiers(held([0xE0, 0xE2])))
        XCTAssertFalse(InputCapture.holdsChordModifiers(held([0xE0, 0xE4, 0xE1, 0xE5])))
        XCTAssertFalse(InputCapture.holdsChordModifiers(held([0xE3, 0xE2, 0xE1])))
    }

    /// The handler compares letters as VKs: O toggles the ring, Q releases, A mutes.
    func testChordLettersAreTheVKsTheirKeysSend() {
        XCTAssertEqual(InputCapture.hidToVK[0x12], 0x4F) // O
        XCTAssertEqual(InputCapture.hidToVK[0x14], 0x51) // Q
        XCTAssertEqual(InputCapture.hidToVK[0x04], 0x41) // A
    }
}
