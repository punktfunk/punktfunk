#if os(macOS)
import XCTest

@testable import PunktfunkKit

/// Pins the macOS flagsChanged → modifier-VK resolution (InputCapture.resolveModifier).
/// Modifier keys arrive only as flagsChanged, which carries no down-vs-up: the changed key
/// is the event's keyCode, and the direction is recovered from the flag bits — with a
/// held-state fallback for keyboards that omit the device-dependent L/R bits (the gap that
/// used to silently drop Control when the transition was diffed from those bits alone).
final class ModifierResolveTests: XCTestCase {
    /// Resolve with a fixed already-held answer for the fallback path.
    private func resolve(
        keyCode: UInt16, rawFlags: UInt, held: Bool = false
    ) -> (vk: UInt32, down: Bool)? {
        InputCapture.resolveModifier(keyCode: keyCode, rawFlags: rawFlags) { _ in held }
    }

    // MARK: Keyboards that report the device-dependent L/R bits (the common case)

    func testControlPressAndReleaseWithDeviceBits() {
        // Real left-Control down: NX_CONTROLMASK | NX_DEVICELCTLKEYMASK (+ misc low bits).
        let down = resolve(keyCode: 59, rawFlags: 0x4_0101)
        XCTAssertEqual(down?.vk, 0xA2) // VK_LCONTROL
        XCTAssertEqual(down?.down, true)
        // Release: the class mask is gone entirely.
        let up = resolve(keyCode: 59, rawFlags: 0x100)
        XCTAssertEqual(up?.vk, 0xA2)
        XCTAssertEqual(up?.down, false)
    }

    func testRightControlUsesItsOwnDeviceBit() {
        let down = resolve(keyCode: 62, rawFlags: 0x4_2000)
        XCTAssertEqual(down?.vk, 0xA3) // VK_RCONTROL
        XCTAssertEqual(down?.down, true)
    }

    func testReleasingOneOfTwoHeldControls() {
        // Left goes up while right stays held: class mask still set, right device bit
        // still set, LEFT device bit cleared — the left key must resolve as UP.
        let leftUp = resolve(keyCode: 59, rawFlags: 0x4_2000, held: true)
        XCTAssertEqual(leftUp?.vk, 0xA2)
        XCTAssertEqual(leftUp?.down, false)
    }

    func testEverySideMapsToItsOwnVK() {
        XCTAssertEqual(resolve(keyCode: 56, rawFlags: 0x2_0002)?.vk, 0xA0) // VK_LSHIFT
        XCTAssertEqual(resolve(keyCode: 60, rawFlags: 0x2_0004)?.vk, 0xA1) // VK_RSHIFT
        XCTAssertEqual(resolve(keyCode: 58, rawFlags: 0x8_0020)?.vk, 0xA4) // VK_LMENU
        XCTAssertEqual(resolve(keyCode: 61, rawFlags: 0x8_0040)?.vk, 0xA5) // VK_RMENU
        XCTAssertEqual(resolve(keyCode: 55, rawFlags: 0x10_0008)?.vk, 0x5B) // VK_LWIN
        XCTAssertEqual(resolve(keyCode: 54, rawFlags: 0x10_0010)?.vk, 0x5C) // VK_RWIN
        for (_, down) in [56, 60, 58, 61, 55, 54].compactMap({
            self.resolve(keyCode: UInt16($0), rawFlags: 0xFF_FFFF)
        }) {
            XCTAssertTrue(down)
        }
    }

    // MARK: Keyboards that DON'T report the device bits (the bug this resolver fixes)

    func testControlPressWithoutDeviceBitsFallsBackToHeldState() {
        // Only NX_CONTROLMASK, no low bits at all: a flag diff of the device bits sees no
        // transition and drops the key — the fallback must infer DOWN from "not held yet".
        let down = resolve(keyCode: 59, rawFlags: 0x4_0000, held: false)
        XCTAssertEqual(down?.vk, 0xA2)
        XCTAssertEqual(down?.down, true)
        // And the mirror release (class cleared) still resolves as UP.
        let up = resolve(keyCode: 59, rawFlags: 0, held: true)
        XCTAssertEqual(up?.down, false)
    }

    func testClassBitStillSetButKeyAlreadyHeldResolvesUp() {
        // Device-bit-less keyboard, second same-class key still holding the class bit:
        // the best available answer for the key that changed is to flip its held state.
        let up = resolve(keyCode: 59, rawFlags: 0x4_0000, held: true)
        XCTAssertEqual(up?.down, false)
    }

    // MARK: Modifiers the host doesn't consume on this path

    func testFnAndCapsLockResolveToNothing() {
        XCTAssertNil(resolve(keyCode: 63, rawFlags: 0x80_0000)) // Fn / Globe
        XCTAssertNil(resolve(keyCode: 57, rawFlags: 0x1_0000)) // Caps Lock
    }

    // MARK: Caps Lock, a lock state sent as a tap

    func testCapsLockTapsOnEveryChangeOfState() {
        let tap = InputCapture.capsLockTap
        // The first state always goes out; the host's lock is unknown until then.
        XCTAssertEqual(tap(57, 0x1_0100, nil).map { [$0.on, $0.tap] }, [true, true])
        XCTAssertEqual(tap(57, 0x100, true).map { [$0.on, $0.tap] }, [false, true])
        XCTAssertEqual(tap(57, 0x1_0100, false).map { [$0.on, $0.tap] }, [true, true])
        // A repeat of the state the host already has toggles nothing.
        XCTAssertEqual(tap(57, 0x1_0100, true).map { [$0.on, $0.tap] }, [true, false])
        // Shift with Caps Lock on is Shift's.
        XCTAssertNil(tap(56, 0x1_0102, true))
    }
}
#endif
