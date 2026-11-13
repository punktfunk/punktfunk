// The management transport's connection state as the pool and the retry loop read it.

import XCTest
@testable import PunktfunkKit

final class MgmtTransportTests: XCTestCase {
    /// The pool reads a connection's health from its actor while the connection's queue marks it
    /// failed. Thread Sanitizer (`swift test --sanitize=thread`) reports the race if it returns.
    func testHealthReadsBesideTheQueueThatFailsIt() async throws {
        let connection = MgmtConnection(host: "127.0.0.1", port: 9, identity: nil, pin: Data())
        let reads = Task.detached {
            let deadline = Date().addingTimeInterval(5)
            while connection.isHealthy, Date() < deadline {}
        }
        _ = try? await connection.perform(path: "/", timeout: 2)
        await reads.value
        XCTAssertFalse(connection.isHealthy)
    }
}
