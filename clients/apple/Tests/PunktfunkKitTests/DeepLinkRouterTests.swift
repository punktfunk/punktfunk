// What a `punktfunk://` link is allowed to do. The connect and browse routes ran these rules
// side by side with no test between them, and had drifted.

import XCTest

@testable import PunktfunkShared

final class DeepLinkRouterTests: XCTestCase {
    private let idle = DeepLinkRouter.SessionState(isIdle: true)

    private func host(
        _ name: String = "Desk", address: String = "10.0.0.2", pin: Data? = nil
    ) -> StoredHost {
        var h = StoredHost(name: name, address: address)
        h.pinnedSHA256 = pin
        return h
    }

    private func link(_ string: String) throws -> DeepLink {
        try DeepLink(url: XCTUnwrap(URL(string: string)))
    }

    func testAnIdNamedHostProceeds() throws {
        let saved = host()
        let out = DeepLinkRouter.resolve(
            link: try link("punktfunk://connect/\(saved.id.uuidString)"),
            hosts: [saved], catalog: PresetCatalog(presets: []), session: idle, browse: false)
        guard case .proceed(let h, let selection) = out else { return XCTFail("got \(out)") }
        XCTAssertEqual(h.id, saved.id)
        XCTAssertEqual(selection, .inherit)
    }

    func testAGuessableNameOnlyEverConfirms() throws {
        // A label is something anything that can open a URL could guess, so it costs a tap.
        let saved = host("Desk")
        let out = DeepLinkRouter.resolve(
            link: try link("punktfunk://connect/Desk"),
            hosts: [saved], catalog: PresetCatalog(presets: []), session: idle, browse: false)
        guard case .confirm = out else { return XCTFail("a guessable ref must confirm, got \(out)") }
    }

    func testAFingerprintThatDisagreesRefuses() throws {
        let saved = host(pin: Data(repeating: 0xAB, count: 32))
        let other = String(repeating: "cd", count: 32)
        let out = DeepLinkRouter.resolve(
            link: try link("punktfunk://connect/\(saved.id.uuidString)?fp=\(other)"),
            hosts: [saved], catalog: PresetCatalog(presets: []), session: idle, browse: false)
        guard case .notice(let text) = out else { return XCTFail("got \(out)") }
        XCTAssertTrue(text.contains("fingerprint"), text)
    }

    func testALiveSessionIsNeverPreempted() throws {
        let saved = host()
        let other = host("Couch", address: "10.0.0.3")
        let busy = DeepLinkRouter.SessionState(
            isIdle: false, activeHostID: other.id, activeHostName: other.displayName)
        let out = DeepLinkRouter.resolve(
            link: try link("punktfunk://connect/\(saved.id.uuidString)"),
            hosts: [saved, other], catalog: PresetCatalog(presets: []),
            session: busy, browse: false)
        guard case .notice(let text) = out else { return XCTFail("got \(out)") }
        XCTAssertEqual(text, String(localized: L10n.linkSessionRunning))
    }

    func testLinkingToTheHostAlreadyStreamingIsANoOp() throws {
        let saved = host()
        let busy = DeepLinkRouter.SessionState(
            isIdle: false, activeHostID: saved.id, activeHostName: saved.displayName)
        let out = DeepLinkRouter.resolve(
            link: try link("punktfunk://connect/\(saved.id.uuidString)"),
            hosts: [saved], catalog: PresetCatalog(presets: []), session: busy, browse: false)
        XCTAssertEqual(out, .alreadyHere)
    }

    func testAnUnknownPresetRefusesRatherThanInheriting() throws {
        // The quiet degrade is the dangerous one: it streams with settings nobody asked for.
        let saved = host()
        let out = DeepLinkRouter.resolve(
            link: try link("punktfunk://connect/\(saved.id.uuidString)?preset=Nope"),
            hosts: [saved], catalog: PresetCatalog(presets: []), session: idle, browse: false)
        guard case .notice(let text) = out else { return XCTFail("got \(out)") }
        XCTAssertTrue(text.contains("Nope"), text)
    }

    func testBothRoutesAgreeOnEveryRefusal() throws {
        // The drift this type exists to stop: same link, same verdict, whichever route asked.
        let saved = host(pin: Data(repeating: 0xAB, count: 32))
        let other = String(repeating: "cd", count: 32)
        // An UNSAVED host is the one deliberate difference (asserted below), so it is not here.
        let cases = [
            "punktfunk://connect/\(saved.id.uuidString)?fp=\(other)",
            "punktfunk://connect/\(saved.id.uuidString)?preset=Nope",
            "punktfunk://connect/Desk?fp=\(other)",
        ]
        for raw in cases {
            let parsed = try link(raw)
            let connect = DeepLinkRouter.resolve(
                link: parsed, hosts: [saved], catalog: PresetCatalog(presets: []),
                session: idle, browse: false)
            let browse = DeepLinkRouter.resolve(
                link: parsed, hosts: [saved], catalog: PresetCatalog(presets: []),
                session: idle, browse: true)
            XCTAssertEqual(connect, browse, "routes disagree about \(raw)")
        }
    }

    func testAnUnsavedHostIsWordedForItsRoute() throws {
        // The one place the two are meant to differ: a library rides the paired identity, so
        // there is nothing to show before the host is saved, while a connect can at least name
        // the address the link pointed at.
        let parsed = try link("punktfunk://connect/nosuchhost")
        let empty = PresetCatalog(presets: [])
        let connect = DeepLinkRouter.resolve(
            link: parsed, hosts: [], catalog: empty, session: idle, browse: false)
        let browse = DeepLinkRouter.resolve(
            link: parsed, hosts: [], catalog: empty, session: idle, browse: true)
        guard case .notice(let c) = connect, case .notice(let b) = browse else {
            return XCTFail("expected notices")
        }
        XCTAssertTrue(c.contains("9777"), "connect names the address it pointed at: \(c)")
        XCTAssertTrue(b.contains("browsed on a saved host"), b)
    }
}
