import Foundation
import XCTest

@testable import PunktfunkShared

/// The start-screen policy's cross-client contract, against
/// `clients/shared/start-screen-vectors.json`.
///
/// Three hand-written resolvers decide where every client opens — this one,
/// `crates/client/pf-client-core/src/start.rs`, and the Android kit's `StartScreen.kt`. A client that
/// disagreed would open on a different host from the one its own settings row names. Sibling of
/// `SharedFoundationTests.testDeepLinkSharedVectors`, read the same way.
final class StartScreenVectorsTests: XCTestCase {
    /// Read from the repo, not from a bundle resource: a copy would be a second file, and a
    /// second file drifts. Four `deletingLastPathComponent()` calls walk
    /// `Tests/PunktfunkKitTests/` → `Tests/` → `apple/` → `clients/`.
    private static var vectorFileURL: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("shared/start-screen-vectors.json")
    }

    private struct VectorFile: Decodable {
        let cases: [Case]
    }

    /// A host reduced to the three things the policy reads. Each store spells the pin
    /// differently — hex here, `Data?` in `StoredHost` — so the file only asks whether one exists.
    private struct Host: Decodable {
        let name: String
        let id: String
        let paired: Bool
        let pinned: Bool
    }

    private struct Case: Decodable {
        let name: String
        let startIn: String
        let defaultHost: String?
        let hosts: [Host]
        let expect: Expect

        enum CodingKeys: String, CodingKey {
            case name
            case startIn = "start_in"
            case defaultHost = "default_host"
            case hosts
            case expect
        }
    }

    private struct Expect: Decodable {
        let start: String
        let hostIndex: Int?
        let source: String

        enum CodingKeys: String, CodingKey {
            case start
            case hostIndex = "host_index"
            case source
        }
    }

    func testEverySharedVectorAgrees() throws {
        let url = Self.vectorFileURL
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: url.path),
            "the shared vector file must be reachable at \(url.path)")
        let file = try JSONDecoder().decode(VectorFile.self, from: Data(contentsOf: url))
        XCTAssertGreaterThanOrEqual(file.cases.count, 10, "the vector file is the contract")

        for c in file.cases {
            // The vectors carry the Rust id grammar (lowercase uuid-ish strings). `StoredHost.id`
            // is a real `UUID`, so map each case id onto a deterministic uuid and carry the SAME
            // mapping into `defaultHost` — which is exactly the pointer the app stores.
            var uuids: [String: UUID] = [:]
            // `StoredHost` has no separate paired flag: the pin IS the pairing here, which is
            // what "Forget Identity" clears. So both vector fields fold into one.
            let usable: [StoredHost] = c.hosts.enumerated().map { i, h in
                let uuid = UUID(uuidString: String(format: "00000000-0000-4000-8000-%012d", i))!
                uuids[h.id] = uuid
                return StoredHost(
                    id: uuid,
                    name: h.name,
                    address: "10.0.0.\(i + 1)",
                    pinnedSHA256: h.paired && h.pinned ? Data([0xAB]) : nil)
            }
            let pointer = c.defaultHost.map { uuids[$0]?.uuidString ?? $0 }

            let (host, source) = StartScreen.defaultHost(id: pointer, hosts: usable)
            XCTAssertEqual(source.rawValue, c.expect.source, "\(c.name) source")
            XCTAssertEqual(
                host.flatMap { h in usable.firstIndex(where: { $0.id == h.id }) },
                c.expect.hostIndex, "\(c.name) host_index")

            let start = StartScreen.resolve(
                startIn: c.startIn, defaultHost: pointer, hosts: usable)
            let got: String
            switch start {
            case .hosts: got = "hosts"
            case .library: got = "library"
            case .stream: got = "stream"
            }
            XCTAssertEqual(got, c.expect.start, "\(c.name) start")
        }
    }

    /// The pointer the app writes is `UUID.uuidString`, which is UPPERCASE; the Rust client mints
    /// lowercase into the same key name. Either spelling must resolve, or a store written on one
    /// platform would silently lose its default on the other.
    func testTheDefaultHostPointerIsCaseInsensitive() {
        let id = UUID()
        let hosts = [
            StoredHost(id: id, name: "Desk", address: "10.0.0.5", pinnedSHA256: Data([0xAB])),
            StoredHost(name: "Couch", address: "10.0.0.6", pinnedSHA256: Data([0xCD])),
        ]
        for spelling in [id.uuidString, id.uuidString.lowercased()] {
            let (host, source) = StartScreen.defaultHost(id: spelling, hosts: hosts)
            XCTAssertEqual(host?.id, id, "\(spelling) did not resolve")
            XCTAssertEqual(source, .explicit)
        }
    }

    /// An unpaired host is never a landing, so a store of them opens the list whatever the
    /// setting says — the rule that carries a fresh install, before anything is paired.
    func testAnUnpairedStoreAlwaysOpensTheList() {
        let hosts = [StoredHost(name: "Desk", address: "10.0.0.5")]
        for value in StartIn.allCases {
            XCTAssertEqual(
                StartScreen.resolve(startIn: value.stored, defaultHost: nil, hosts: hosts),
                .hosts, "\(value.stored) landed somewhere with nothing to land on")
        }
        XCTAssertEqual(StartIn.parse("shelf"), .hosts, "unknown must degrade, not throw")
        XCTAssertEqual(StartIn.parse(nil), .hosts)
    }
}
