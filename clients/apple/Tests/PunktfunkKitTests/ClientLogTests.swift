// The client log ring behind "Send logs to host", and the POST framing that carries it.

import XCTest
@testable import PunktfunkKit

final class ClientLogTests: XCTestCase {
    /// The ring is process-global, so this single test owns the whole lifecycle (parallel tests
    /// over one global would interleave) — the same shape as `pf_client_core::logring`'s test.
    func testRingBoundsAndRendersWithEvictionNote() {
        let marker = "ringtest-\(ProcessInfo.processInfo.processIdentifier)"
        for i in 0..<(ClientLogRing.maxLines + 10) {
            ClientLogRing.note("\(marker) line \(i)")
        }
        let text = ClientLogRing.render(header: "punktfunk-apple test")
        XCTAssertTrue(text.hasPrefix("punktfunk-apple test\n"))
        XCTAssertTrue(text.contains("older lines evicted from the ring"))
        XCTAssertFalse(text.contains("\n\(marker) line 0\n"), "oldest line survived eviction")
        XCTAssertTrue(text.hasSuffix("\(marker) line \(ClientLogRing.maxLines + 9)\n"))
        XCTAssertLessThanOrEqual(text.utf8.count, ClientLogRing.maxBytes + 256)

        // A pathological line is truncated, not ring-flushing — and cut safely mid-scalar.
        ClientLogRing.note(String(repeating: "é", count: 10_000))
        let after = ClientLogRing.render(header: "h")
        XCTAssertTrue(after.contains("…"))
        XCTAssertTrue(after.hasSuffix("…\n"))

        // The drop-in logger formats `stamp LEVEL category message` and honours the OSLogMessage
        // options the call sites use; debug stays out of the ring.
        let log = ClientLog(category: "test")
        log.info("\(marker) value \(1.23456, format: .fixed(precision: 2)) \(42, privacy: .public)")
        log.debug("\(marker) debug-only")
        let lines = ClientLogRing.render(header: "h").components(separatedBy: "\n")
        let info = lines.last { $0.contains("\(marker) value") }
        XCTAssertNotNil(info)
        XCTAssertTrue(info!.contains(" INFO  test \(marker) value 1.23 42"), info!)
        // `2026-08-15T12:03:47.123Z ` leads — wall time, so a bundle lines up with the host log.
        XCTAssertNotNil(
            info!.range(of: #"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z INFO  test "#, options: .regularExpression),
            info!)
        XCTAssertFalse(lines.contains { $0.contains("debug-only") })
    }

    /// A run's file is the next run's `previous`, marked under its header, and stays under its
    /// cap with the newest lines kept.
    func testFileKeepsThePreviousRunAndTrims() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("client-log-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }

        let first = ClientLogFile(directory: dir, maxBytes: 4096, header: "h1")
        for i in 0..<1000 { first.append("line \(i)") }
        first.flush()
        let size = try FileManager.default.attributesOfItem(atPath: first.current.path)[.size] as? Int
        XCTAssertLessThanOrEqual(try XCTUnwrap(size), 4096)
        let text = try String(contentsOf: first.current, encoding: .utf8)
        XCTAssertTrue(text.hasPrefix("h1\n… older lines trimmed"), text)
        XCTAssertTrue(text.hasSuffix("\nline 999\n"))
        XCTAssertFalse(text.contains("\nline 0\n"))
        XCTAssertNil(first.previousRun())

        let second = ClientLogFile(directory: dir, maxBytes: 4096, header: "h2")
        let previous = try XCTUnwrap(second.previousRun())
        XCTAssertTrue(previous.hasPrefix("h1\n… the app's previous run"), previous)
        XCTAssertTrue(previous.hasSuffix("\nline 999\n"))
        XCTAssertEqual(try String(contentsOf: second.current, encoding: .utf8), "h2\n")
        second.dropPrevious()
        XCTAssertNil(second.previousRun())
    }

    func testHeaderNamesTheAppAndPlatform() {
        let header = ClientLogRing.header()
        XCTAssertTrue(header.hasPrefix("punktfunk-apple "))
        XCTAssertTrue(header.hasSuffix(" — client log bundle"))
        #if os(macOS)
        XCTAssertTrue(header.contains("macOS"))
        #endif
    }

    func testPostIsLengthFramedAndGetHasNoBody() {
        let body = Data("hello ring\n".utf8)
        let post = String(decoding: MgmtConnection.requestBytes(
            host: "fd00::1", port: 47990, method: "POST", path: "/api/v1/client-logs",
            body: body, contentType: "text/plain; charset=utf-8"), as: UTF8.self)
        XCTAssertTrue(post.hasPrefix("POST /api/v1/client-logs HTTP/1.1\r\nHost: [fd00::1]:47990\r\n"))
        XCTAssertTrue(post.contains("\r\nContent-Type: text/plain; charset=utf-8\r\n"))
        XCTAssertTrue(post.contains("\r\nContent-Length: \(body.count)\r\n\r\nhello ring\n"))
        XCTAssertTrue(post.hasSuffix("\r\n\r\nhello ring\n"))

        let get = String(decoding: MgmtConnection.requestBytes(
            host: "192.168.1.2", port: 47990, method: "GET", path: "/api/v1/library",
            body: nil, contentType: nil), as: UTF8.self)
        XCTAssertTrue(get.hasPrefix("GET /api/v1/library HTTP/1.1\r\nHost: 192.168.1.2:47990\r\n"))
        XCTAssertFalse(get.contains("Content-Length"))
        XCTAssertTrue(get.hasSuffix("\r\n\r\n"))
    }
}
