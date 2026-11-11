// The ring editors' edits on `OverlayConfig`: both the touch and the TV editor call these.

import XCTest
@testable import PunktfunkShared

final class OverlayConfigEditTests: XCTestCase {
    func testAssignPutsAnActionInASlotAndAnUnknownIdEmptiesIt() {
        var cfg = OverlayConfig.platformDefault(.tv)
        cfg.assign(5, id: "host:power.sleep")
        XCTAssertEqual(cfg.ring[5], .host("power.sleep"))
        cfg.assign(0, id: "")
        XCTAssertNil(cfg.ring[0], "the catalogue's empty slot")
        cfg.assign(1, id: "warp_drive")
        XCTAssertNil(cfg.ring[1], "an id this build does not know")
        XCTAssertEqual(cfg.ring.count, OverlayConfig.ringSlots)
    }

    func testSwapTradesTwoSlotsAndOneSlotOntoItselfChangesNothing() {
        var cfg = OverlayConfig.platformDefault(.tv)
        cfg.swap(0, 3)
        XCTAssertEqual(cfg.ring, [.guide, .disconnectLinger, .stats, .endStream, .qam, nil])
        cfg.swap(4, 5)
        XCTAssertEqual(cfg.ring[4], nil)
        XCTAssertEqual(cfg.ring[5], .qam)
        let before = cfg
        cfg.swap(2, 2)
        XCTAssertEqual(cfg, before)
    }
}
