// WakePolicy: the one wake rule the touch UI, the console and the intent share.

import XCTest

@testable import PunktfunkKit

final class WakePolicyTests: XCTestCase {
    private let macs = ["aa:bb:cc:dd:ee:ff"]

    /// A Wake Host action needs a MAC to send to; the switch is not asked (the explicit wake
    /// works with Auto-wake off).
    func testCanWakeNeedsAMac() {
        XCTAssertTrue(WakePolicy.canWake(macs))
        XCTAssertFalse(WakePolicy.canWake([]))
    }

    /// Every input of the automatic wake, each one able to veto it.
    func testAutoWakeNeedsTheSwitchAndAMac() {
        for autoWake in [false, true] {
            for known in [[], macs] {
                XCTAssertEqual(
                    WakePolicy.autoWakes(known, autoWake: autoWake),
                    autoWake && !known.isEmpty,
                    "\(autoWake) \(known)")
            }
        }
    }

    /// 0d8b710cd: with Auto-wake off a library open sends nothing, yet the host can still be
    /// offered a Wake Host.
    func testAutoWakeOffLeavesTheExplicitWake() {
        XCTAssertFalse(WakePolicy.autoWakes(macs, autoWake: false))
        XCTAssertTrue(WakePolicy.canWake(macs))
    }

    /// f33badd4a: a host with no MAC is never woken, so a load asks it once.
    func testNoMacNeverWakes() {
        XCTAssertFalse(WakePolicy.autoWakes([], autoWake: true))
    }

    /// A connect wakes first only where Auto-wake would, and never a host the probe reached.
    func testWakesFirstSkipsAHostTheProbeReached() {
        for autoWake in [false, true] {
            for known in [[], macs] {
                for online in [false, true] {
                    XCTAssertEqual(
                        WakePolicy.wakesFirst(known, autoWake: autoWake, probedOnline: online),
                        autoWake && !known.isEmpty && !online,
                        "\(autoWake) \(known) \(online)")
                }
            }
        }
    }
}
