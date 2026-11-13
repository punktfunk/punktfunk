import XCTest

import PunktfunkCore
@testable import PunktfunkKit

/// The core assembles AUs in the newest lender it has been given, on its own thread, some time
/// after the install. A lender may go only once a later one has been asked for a buffer and every
/// buffer it lent is back: a call into a freed lender is a use after free.
final class AccessUnitLendersTests: XCTestCase {
    /// The callbacks the core is left holding after `set_au_memory`.
    private func install(_ lenders: AccessUnitLenders, _ memory: AccessUnitMemory) throws
        -> PunktfunkAuMemory
    {
        var installed: PunktfunkAuMemory?
        XCTAssertTrue(lenders.lend(memory) { installed = $0; return true })
        return try XCTUnwrap(installed)
    }

    /// One buffer asked for the way the core does; nil when the lender refuses.
    private func take(_ memory: PunktfunkAuMemory, count: UInt = 1000) throws -> PunktfunkAuStore? {
        var store = PunktfunkAuStore(data: nil, len: 0, token: 0)
        let take = try XCTUnwrap(memory.take)
        return take(memory.userdata, count, &store) ? store : nil
    }

    private func give(back store: PunktfunkAuStore, to memory: PunktfunkAuMemory) throws {
        try XCTUnwrap(memory.release)(memory.userdata, store.token)
    }

    func testALenderStaysUntilALaterOneIsAsked() throws {
        let lenders = AccessUnitLenders()
        let first = try install(lenders, AccessUnitSlotPool(capacity: 4096))
        _ = try install(lenders, AccessUnitSlotPool(capacity: 4096))
        // The core may not have installed the second yet, so it can still ask the first.
        lenders.sweep()
        XCTAssertEqual(lenders.retained, 2)
        try give(back: XCTUnwrap(take(first)), to: first)
        lenders.sweep()
        XCTAssertEqual(lenders.retained, 2)
    }

    func testALenderStaysWhileABufferIsOutAndGoesWhenItIsBack() throws {
        let lenders = AccessUnitLenders()
        weak var firstPool: AccessUnitSlotPool?
        let first: PunktfunkAuMemory
        let held: PunktfunkAuStore
        do {
            let pool = AccessUnitSlotPool(capacity: 4096)
            firstPool = pool
            first = try install(lenders, pool)
            held = try XCTUnwrap(take(first))
        }
        let second = try install(lenders, AccessUnitSlotPool(capacity: 4096))
        let secondBuffer = try XCTUnwrap(take(second))

        lenders.sweep()
        XCTAssertEqual(lenders.retained, 2, "a buffer of the first is still the core's")
        XCTAssertNotNil(firstPool)

        try give(back: held, to: first)
        lenders.sweep()
        XCTAssertEqual(lenders.retained, 1)
        XCTAssertNil(firstPool, "the first lender's pool outlived its last buffer")

        // The second is the one the core assembles in, and stays.
        try give(back: secondBuffer, to: second)
        lenders.sweep()
        XCTAssertEqual(lenders.retained, 1)
    }

    func testARefusedTakeHoldsNothing() throws {
        let lenders = AccessUnitLenders()
        let first = try install(lenders, AccessUnitSlotPool(capacity: 4096))
        XCTAssertNil(try take(first, count: 1 << 20), "past the pool's capacity the AU takes the heap")
        let second = try install(lenders, AccessUnitSlotPool(capacity: 4096))
        _ = try XCTUnwrap(take(second))
        lenders.sweep()
        XCTAssertEqual(lenders.retained, 1)
    }

    func testALenderTheCoreRefusedIsNotKept() {
        let lenders = AccessUnitLenders()
        XCTAssertFalse(lenders.lend(AccessUnitSlotPool(capacity: 4096)) { _ in false })
        XCTAssertEqual(lenders.retained, 0)
    }

    func testRemoveAllDropsEveryLender() throws {
        let lenders = AccessUnitLenders()
        _ = try install(lenders, AccessUnitSlotPool(capacity: 4096))
        _ = try install(lenders, AccessUnitSlotPool(capacity: 4096))
        lenders.removeAll()
        XCTAssertEqual(lenders.retained, 0)
    }
}
