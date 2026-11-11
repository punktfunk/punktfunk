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

    /// Three shortcuts on a ring and a pad binding that sends the third.
    private func threeShortcuts() -> (OverlayConfig, Shortcuts) {
        var cfg = OverlayConfig.platformDefault(.tv)
        for keys in [["f1"], ["f2"], ["ctrl", "f3"]] {
            cfg.saveShortcut(OverlayShortcut(id: cfg.nextShortcutID(bindings: Shortcuts()), keys: keys))
        }
        let bindings = Shortcuts(bindings: [
            .init(trigger: .pad(buttons: [0x0020, 0x8000], hold: false), action: .slot(.shortcut("s3"))),
            .init(trigger: .pad(buttons: [0x0020, 0x1000], hold: false), action: .ring),
        ])
        return (cfg, bindings)
    }

    /// R7-1: a binding left on a removed shortcut's id never fires the next new one.
    func testANewShortcutSkipsAnIdABindingStillNames() {
        var (cfg, bindings) = threeShortcuts()
        XCTAssertEqual(cfg.shortcuts.map(\.id), ["s1", "s2", "s3"])
        var other = bindings
        cfg.removeShortcut("s3")
        XCTAssertEqual(cfg.nextShortcutID(bindings: bindings), "s4", "the binding is in another layer")
        other.bindings.removeAll()
        XCTAssertEqual(cfg.nextShortcutID(bindings: other), "s3", "no binding names it")
        bindings.bindings[0].action = .slot(.shortcut("s9"))
        XCTAssertEqual(cfg.nextShortcutID(bindings: bindings), "s10")
    }
}
