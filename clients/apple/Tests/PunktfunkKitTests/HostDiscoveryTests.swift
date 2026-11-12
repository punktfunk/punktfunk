// Advertise a fake punktfunk/1 host over real mDNS (NWListener) and assert HostDiscovery's
// NWBrowser finds it, resolves an address+port, and parses the TXT (id / pair / fp). This
// exercises the whole client discovery path on the loopback/LAN; it self-skips if the test
// environment blocks Bonjour (sandboxed CI without local-network access).

import Network
import PunktfunkKit
import XCTest

final class HostDiscoveryTests: XCTestCase {
    func testFindsAdvertisedHost() async throws {
        let serviceName = "PunktfunkTest-\(UUID().uuidString.prefix(8))"
        let uniqueid = "test-\(UUID().uuidString)"

        var txt = NWTXTRecord()
        txt["proto"] = "punktfunk/2"
        txt["fp"] = String(repeating: "ab", count: 32) // 64 hex chars, like a real cert SHA-256
        txt["pair"] = "required"
        txt["id"] = uniqueid

        let listener = try NWListener(using: .udp)
        listener.service = NWListener.Service(
            name: String(serviceName), type: "_punktfunk._udp", txtRecord: txt)
        // The resolver opens a throwaway UDP flow to read the resolved endpoint — accept and
        // drop it so it doesn't linger.
        listener.newConnectionHandler = { connection in connection.cancel() }
        listener.start(queue: .global())
        defer { listener.cancel() }

        let discovery = await HostDiscovery()
        await discovery.start()
        defer { Task { await discovery.stop() } }

        // Poll up to ~10s for the advert to be browsed AND resolved.
        var found: DiscoveredHost?
        let deadline = Date().addingTimeInterval(10)
        while Date() < deadline {
            if let host = await discovery.hosts.first(where: { $0.name == String(serviceName) }) {
                found = host
                break
            }
            try await Task.sleep(nanoseconds: 200_000_000)
        }

        guard let host = found else {
            throw XCTSkip("mDNS discovery unavailable in this environment (no local network).")
        }
        XCTAssertEqual(host.id, uniqueid, "the stable mDNS id should key the host")
        XCTAssertTrue(host.requiresPairing, "pair=required must surface as requiresPairing")
        XCTAssertEqual(host.fingerprintHex, String(repeating: "ab", count: 32))
        XCTAssertFalse(host.host.isEmpty, "a resolved address is required to connect")
        XCTAssertGreaterThan(host.port, 0, "a resolved port is required to connect")

        // A rescan tears the browser down and re-arms it (the only way past the iOS local-network
        // permission gate without relaunching). The host must come BACK — `refresh()` cancels every
        // in-flight resolve and invalidates the previous generation's callbacks, so a re-arm that
        // failed to re-drive them would leave the list permanently empty.
        await discovery.rescan()
        var reappeared = false
        let rescanDeadline = Date().addingTimeInterval(10)
        while Date() < rescanDeadline {
            if await discovery.hosts.contains(where: { $0.id == uniqueid }) {
                reappeared = true
                break
            }
            try await Task.sleep(nanoseconds: 200_000_000)
        }
        XCTAssertTrue(reappeared, "a rescan must re-find a host that is still advertising")
    }
}
