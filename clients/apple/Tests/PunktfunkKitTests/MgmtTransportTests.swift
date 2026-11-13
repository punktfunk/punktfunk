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

    /// A reused socket that failed as a socket goes again; a timeout or a bad answer never does,
    /// since the host may already have run the request.
    func testRetriesOnlyAReusedSocketThatFailedAsASocket() {
        let dropped = MgmtTransportError.connection("reset")
        XCTAssertTrue(MgmtTransport.retries(after: dropped, reused: true))
        XCTAssertFalse(MgmtTransport.retries(after: dropped, reused: false))
        XCTAssertFalse(MgmtTransport.retries(after: MgmtTransportError.timedOut, reused: true))
        XCTAssertFalse(MgmtTransport.retries(after: MgmtTransportError.tooLarge, reused: true))
    }

    /// A kept-alive socket the host closed while idle hangs up without a byte: that reads as a
    /// socket error, so the reused connection retries. A partial answer does not.
    func testAHangUpBeforeAnyAnswerIsASocketError() throws {
        XCTAssertThrowsError(try MgmtConnection.closeFramed(Data())) {
            XCTAssertTrue(MgmtTransport.retries(after: $0, reused: true))
        }
        XCTAssertThrowsError(try MgmtConnection.closeFramed(Data("HTTP/1.1 200 OK\r\nContent-".utf8))) {
            XCTAssertFalse(MgmtTransport.retries(after: $0, reused: true))
        }
        let whole = try MgmtConnection.closeFramed(Data("HTTP/1.1 200 OK\r\n\r\nbody".utf8))
        XCTAssertEqual(whole.body, Data("body".utf8))
    }
}
