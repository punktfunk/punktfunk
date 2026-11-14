// The management transport's connection state as the pool and the retry loop read it.

import os
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

    /// A host's blocking tunnel calls hold a bounded number of threads, however many callers
    /// ask at once.
    func testTunnelCallsShareAFewThreadsPerHost() async throws {
        let host = "cap-\(UUID().uuidString)"
        let gauge = OSAllocatedUnfairLock(initialState: (now: 0, peak: 0))
        try await withThrowingTaskGroup(of: Void.self) { group in
            for _ in 0..<(MgmtTransport.tunnelWidth + 16) {
                group.addTask {
                    try await MgmtTransport.onTunnelThread(host: host) {
                        gauge.withLock { $0.now += 1; $0.peak = max($0.peak, $0.now) }
                        Thread.sleep(forTimeInterval: 0.05)
                        gauge.withLock { $0.now -= 1 }
                    }
                }
            }
            try await group.waitForAll()
        }
        XCTAssertLessThanOrEqual(gauge.withLock { $0.peak }, MgmtTransport.tunnelWidth)
    }

    /// A caller cancelled while the host's threads are busy gives up its turn without running.
    func testCancelledTunnelWaiterNeverRuns() async throws {
        let host = "cancel-\(UUID().uuidString)"
        let running = OSAllocatedUnfairLock(initialState: 0)
        let gate = DispatchSemaphore(value: 0)
        let busy = (0..<MgmtTransport.tunnelWidth).map { _ in
            Task {
                try await MgmtTransport.onTunnelThread(host: host) {
                    running.withLock { $0 += 1 }
                    gate.wait()
                }
            }
        }
        for _ in 0..<500 where running.withLock({ $0 }) < MgmtTransport.tunnelWidth {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(running.withLock { $0 }, MgmtTransport.tunnelWidth)
        let ran = OSAllocatedUnfairLock(initialState: false)
        let waiter = Task {
            try await MgmtTransport.onTunnelThread(host: host) { ran.withLock { $0 = true } }
        }
        waiter.cancel()
        for _ in busy { gate.signal() }
        do {
            try await waiter.value
            XCTFail("a cancelled waiter ran")
        } catch {
            XCTAssertTrue(error is CancellationError)
        }
        for task in busy { try await task.value }
        XCTAssertFalse(ran.withLock { $0 })
    }
}
