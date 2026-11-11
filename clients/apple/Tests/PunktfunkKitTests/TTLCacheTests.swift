// TTLCache: when a host store asks again, and what a fetch's outcome does to the answer it shows.

import XCTest

@testable import PunktfunkKit

final class TTLCacheTests: XCTestCase {
    private let t0 = Date(timeIntervalSinceReferenceDate: 1_000)

    private func at(_ seconds: TimeInterval) -> Date { t0.addingTimeInterval(seconds) }

    func testAnAskWaitsOutTheTTL() {
        var cache = TTLCache<String>(ttl: 20)
        XCTAssertNotNil(cache.ask("a", now: at(0)))
        XCTAssertNil(cache.ask("a", now: at(19)), "stamped before the fetch")
        XCTAssertNotNil(cache.ask("b", now: at(19)), "keys are apart")
        XCTAssertNotNil(cache.ask("a", now: at(20)))
    }

    func testAnAnswerReplacesTheValue() {
        var cache = TTLCache<String>(ttl: 20)
        let first = cache.ask("a", now: at(0))!
        XCTAssertNil(cache["a"])
        cache.land(first, "Dota 2")
        XCTAssertEqual(cache["a"], "Dota 2")
        let second = cache.ask("a", now: at(20))!
        cache.land(second, "")
        XCTAssertEqual(cache["a"], "")
    }

    func testAStoredAnswerIsFresh() {
        var cache = TTLCache<String>(ttl: 20)
        cache.store("Dota 2", for: "a", now: at(0))
        XCTAssertEqual(cache["a"], "Dota 2")
        XCTAssertNil(cache.ask("a", now: at(10)))
    }

    func testAnInvalidateAsksAgainAtOnce() {
        var cache = TTLCache<String>(ttl: 20)
        cache.land(cache.ask("a", now: at(0))!, "Dota 2")
        cache.invalidate("a")
        XCTAssertNil(cache["a"])
        XCTAssertNotNil(cache.ask("a", now: at(1)))
    }

    /// A screen pick keeps its rows shown until the host's new answer lands.
    func testAnInvalidateCanKeepTheValue() {
        var cache = TTLCache<String>(ttl: 20)
        cache.land(cache.ask("a", now: at(0))!, "rows")
        cache.invalidate("a", keepValue: true)
        XCTAssertEqual(cache["a"], "rows")
        XCTAssertNotNil(cache.ask("a", now: at(1)))
    }
}
