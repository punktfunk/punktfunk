// PunktfunkShared is a wire contract between the app (writer) and the widget extension +
// deep-link senders (readers). These pin the formats that cross that boundary:
//   • the `StoredHost` JSON codec — the widget decodes the exact bytes the app persisted, and
//     older saved JSON (missing `mgmtPort` / `macAddresses` / `presetID`) must still decode;
//   • the `punktfunk://` deep-link grammar — driven by `clients/shared/deeplink-vectors.json`,
//     the SAME file the Rust suite runs, so the two parsers cannot drift into two security
//     postures;
//   • the settings-preset catalog — including the don't-clobber rule that keeps an older build
//     from erasing a newer one's overlay fields just by opening a preset.

import SwiftUI
import XCTest

@testable import PunktfunkKit
import PunktfunkShared

final class SharedFoundationTests: XCTestCase {
    // MARK: - StoredHost JSON codec

    func testStoredHostRoundTrips() throws {
        let host = StoredHost(
            id: UUID(uuidString: "11111111-2222-3333-4444-555555555555")!,
            name: "Tower", address: "192.168.1.173", port: 9777,
            pinnedSHA256: Data([0xDE, 0xAD, 0xBE, 0xEF]),
            lastConnected: Date(timeIntervalSince1970: 1_700_000_000),
            mgmtPort: 47990, macAddresses: ["aa:bb:cc:dd:ee:ff"], clipboardSync: true,
            presetID: "a1b2c3d4e5f6", pinnedPresetIDs: ["0f0f0f0f0f0f"],
            addedAt: Date(timeIntervalSince1970: 1_600_000_000),
            osChain: "linux/fedora/bazzite", previousAddresses: ["100.64.0.7"],
            gamePresets: ["steam:374320": "0f0f0f0f0f0f"])

        let data = try JSONEncoder().encode(host)
        let decoded = try JSONDecoder().decode(StoredHost.self, from: data)
        XCTAssertEqual(decoded, host)
        XCTAssertEqual(decoded.gamePresets, ["steam:374320": "0f0f0f0f0f0f"])
        XCTAssertEqual(decoded.osChain, "linux/fedora/bazzite")
    }

    /// Older saved hosts predate `mgmtPort`/`macAddresses` — and now `presetID`/
    /// `pinnedPresetIDs` too. A missing key must decode to nil, not throw (`init(from:)` reads
    /// every Optional with `decodeIfPresent`). This is the forward-compat guarantee the widget depends
    /// on when reading a store written by any prior build.
    func testStoredHostDecodesLegacyJSONWithoutOptionalKeys() throws {
        let json = """
        {"id":"11111111-2222-3333-4444-555555555555","name":"Old","address":"10.0.0.5","port":9777}
        """.data(using: .utf8)!

        let decoded = try JSONDecoder().decode(StoredHost.self, from: json)
        XCTAssertEqual(decoded.name, "Old")
        XCTAssertNil(decoded.mgmtPort)
        XCTAssertNil(decoded.macAddresses)
        XCTAssertNil(decoded.pinnedSHA256)
        XCTAssertNil(decoded.lastConnected)
        XCTAssertNil(decoded.clipboardSync)
        XCTAssertNil(decoded.presetID)
        XCTAssertNil(decoded.pinnedPresetIDs)
        XCTAssertNil(decoded.addedAt)
        XCTAssertNil(decoded.osChain)
        XCTAssertNil(decoded.previousAddresses)
        XCTAssertNil(decoded.gamePresets)
        // Resolvers fall back cleanly.
        XCTAssertEqual(decoded.effectiveMgmtPort, punktfunkDefaultMgmtPort)
        XCTAssertEqual(decoded.wakeMacs, [])
        XCTAssertEqual(decoded.displayName, "Old")
    }

    /// The addresses a host left are kept, newest first, without repeats, three at most.
    func testAMovedHostRemembersTheAddressesItLeft() {
        var host = StoredHost(name: "Desk", address: "100.64.0.7")
        host.move(to: "192.168.1.9", port: 9777)
        host.move(to: "100.64.0.7", port: 9777)
        XCTAssertEqual(host.previousAddresses, ["192.168.1.9"])
        for a in ["10.0.0.1", "10.0.0.2", "10.0.0.3"] { host.move(to: a, port: 9777) }
        XCTAssertEqual(host.previousAddresses, ["10.0.0.2", "10.0.0.1", "100.64.0.7"])
        XCTAssertEqual(host.address, "10.0.0.3")
    }

    func testStoredHostDisplayNameFallsBackToAddress() {
        let host = StoredHost(name: "", address: "10.0.0.9")
        XCTAssertEqual(host.displayName, "10.0.0.9")
    }

    // MARK: - OS-identity chain (mDNS `os` TXT → card icon walk)

    /// Mirrors pf-client-core's `os.rs` tests — the two implementations must agree on the
    /// grammar (lowercase `[a-z0-9._-]` tokens, ≤ 32 chars each, ≤ 5 of them) and the walk
    /// order (most-specific-first, `macos`→`apple`, `steamos`→`steam`).
    func testOsChainSanitizeAndWalk() {
        XCTAssertEqual(sanitizeOsChain("linux/fedora/bazzite"), "linux/fedora/bazzite")
        XCTAssertEqual(sanitizeOsChain("Linux/Fe do!ra"), "linux/fedora")
        XCTAssertEqual(sanitizeOsChain("///"), "")
        XCTAssertEqual(sanitizeOsChain("a/b/c/d/e/f/g"), "a/b/c/d/e")
        XCTAssertEqual(sanitizeOsChain(String(repeating: "x", count: 80)),
                       String(repeating: "x", count: 32))

        XCTAssertEqual(osIconTokens("linux/fedora/bazzite"), ["bazzite", "fedora", "linux"])
        XCTAssertEqual(osIconTokens("linux/arch/steamos"), ["steam", "arch", "linux"])
        XCTAssertEqual(osIconTokens("macos"), ["apple"])
        XCTAssertEqual(osIconTokens(nil), [])
        XCTAssertEqual(osIconTokens("!!!"), [])
    }

    // MARK: - DeepLink grammar (the cross-language vector file)

    private struct VectorFile: Decodable {
        struct Case: Decodable {
            let name: String
            let url: String
            let error: String?
            let emit: String?
            let expect: Expect?
        }

        struct Expect: Decodable {
            let route: String
            // swiftlint:disable:next identifier_name
            let host_ref: String
            let fp: String?
            let launch: String?
            let preset: String?
            let name: String?
            let host_addr: String?
            let host_port: Int?
        }

        let cases: [Case]
    }

    /// `clients/shared/deeplink-vectors.json`, read from the source tree rather than copied into
    /// the test bundle — a copy is a second file, and a second file drifts. Resolved from
    /// `#filePath` (this file sits at `clients/apple/Tests/PunktfunkKitTests/`).
    private static var vectorFileURL: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/deeplink-vectors.json")
    }

    /// Every case in the shared vector file — the same 44 the Rust suite's `shared_vectors` test
    /// consumes. Refusal CODES are part of the contract, not just the happy path: a parser that
    /// rejects the right inputs for the wrong reason has already drifted from the other one.
    func testDeepLinkSharedVectors() throws {
        let url = Self.vectorFileURL
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: url.path),
            "the cross-language vector file must be readable at \(url.path)")
        let file = try JSONDecoder().decode(VectorFile.self, from: Data(contentsOf: url))
        XCTAssertGreaterThan(file.cases.count, 20, "the vector file is the contract; keep it rich")

        for testCase in file.cases {
            if let code = testCase.error {
                do {
                    let link = try DeepLink.parse(testCase.url)
                    XCTFail("\(testCase.name): expected \(code), parsed \(link)")
                } catch let error as DeepLinkError {
                    XCTAssertEqual(error.code, code, testCase.name)
                }
                continue
            }
            let want = try XCTUnwrap(testCase.expect, testCase.name)
            let link = try DeepLink.parse(testCase.url)
            XCTAssertEqual(link.route.rawValue, want.route, testCase.name)
            XCTAssertEqual(link.hostRef, want.host_ref, testCase.name)
            XCTAssertEqual(link.fp, want.fp, "\(testCase.name) fp")
            XCTAssertEqual(link.launch, want.launch, "\(testCase.name) launch")
            XCTAssertEqual(link.preset, want.preset, "\(testCase.name) preset")
            XCTAssertEqual(link.name, want.name, "\(testCase.name) name")
            XCTAssertEqual(link.host?.address, want.host_addr, "\(testCase.name) host_addr")
            XCTAssertEqual(
                link.host.map { Int($0.port) }, want.host_port, "\(testCase.name) host_port")
            if let emit = testCase.emit {
                XCTAssertEqual(link.urlString, emit, "\(testCase.name) emit")
            }
        }
    }

    /// The widget's and the Connect intent's emitter — a bare UUID path, which is the grammar the
    /// shipped links use. Backward compatibility is not optional here: a widget on the Home Screen
    /// keeps sending yesterday's URL.
    func testDeepLinkConnectRoundTrips() throws {
        let id = UUID()
        let link = DeepLink.connect(host: id)
        XCTAssertEqual(try DeepLink(url: link.url), link)
        XCTAssertEqual(link.hostRef, id.uuidString)
        XCTAssertNil(link.launch)

        let launched = DeepLink.connect(host: id, launchID: "steam:570")
        XCTAssertEqual(try DeepLink(url: launched.url).launch, "steam:570")

        let withPreset = DeepLink.connect(host: id, launchID: nil, preset: "a1b2c3d4e5f6")
        XCTAssertEqual(try DeepLink(url: withPreset.url).preset, "a1b2c3d4e5f6")
    }

    /// The library widget's and the Open Library intent's emitter — the reserved `browse` route
    /// with a bare UUID path. Same backward-compatibility stakes as connect: a Home-Screen widget
    /// keeps sending yesterday's URL.
    func testDeepLinkBrowseRoundTrips() throws {
        let id = UUID(uuidString: "11111111-2222-4333-8444-555555555555")!
        let link = DeepLink.browse(host: id)
        XCTAssertEqual(link.route, .browse)
        XCTAssertEqual(
            link.urlString, "punktfunk://browse/11111111-2222-4333-8444-555555555555")
        XCTAssertEqual(try DeepLink(url: link.url), link)
    }

    /// Self-emitted links ("Copy link", a shortcut) carry all three references, so they survive
    /// both a re-addressed host and a wiped store.
    func testDeepLinkForHostCarriesIDAddressAndPin() throws {
        var host = StoredHost(
            id: UUID(uuidString: "11111111-2222-4333-8444-555555555555")!,
            name: "Desk", address: "192.168.1.50", port: 7777)
        host.pinnedSHA256 = Data(repeating: 0xCC, count: 32)
        let link = DeepLink.forHost(host, launch: "steam:570", preset: "aaaaaaaaaaaa")
        XCTAssertEqual(
            link.urlString,
            "punktfunk://connect/11111111-2222-4333-8444-555555555555"
                + "?fp=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                + "&host=192.168.1.50:7777&launch=steam:570&preset=aaaaaaaaaaaa"
                + "&profile=aaaaaaaaaaaa")
        XCTAssertEqual(try DeepLink.parse(link.urlString), link)
    }

    /// Resolution order — id beats a unique name beats an address — plus the two refusals a
    /// front-end must surface rather than guess through, and the rule that keeps a guessable
    /// reference from dialing: only the record id resolves to `.known`.
    func testDeepLinkHostResolution() throws {
        let desk = StoredHost(
            id: UUID(uuidString: "11111111-2222-4333-8444-555555555555")!,
            name: "Desk", address: "192.168.1.50",
            pinnedSHA256: Data(repeating: 0xAA, count: 32))
        let couchA = StoredHost(name: "Couch", address: "192.168.1.60")
        let couchB = StoredHost(name: "Couch", address: "192.168.1.61")
        let hosts = [desk, couchA, couchB]
        func resolve(_ url: String) throws -> DeepLink.HostResolution {
            try DeepLink.parse(url).resolveHost(in: hosts)
        }

        XCTAssertEqual(
            try resolve("punktfunk://connect/11111111-2222-4333-8444-555555555555"), .known(desk))
        // The id is a UUID nothing can guess; a display name and a LAN address are guesses any web
        // page can make. So the id — and only the id — dials unattended; everything else that finds
        // a saved host stops at the confirmation.
        XCTAssertEqual(try resolve("punktfunk://connect/desk"), .confirm(desk))
        XCTAssertEqual(try resolve("punktfunk://connect/DESK"), .confirm(desk))
        XCTAssertEqual(try resolve("punktfunk://connect/couch"), .ambiguous)
        XCTAssertEqual(try resolve("punktfunk://connect/192.168.1.50:9777"), .confirm(desk))
        XCTAssertEqual(
            try resolve("punktfunk://connect/desk?launch=steam:570"), .confirm(desk))
        // A stale id with the recovery parameter: the address finds the record anyway — and, being
        // an address, behind the confirmation exactly as its own doc always said.
        XCTAssertEqual(
            try resolve(
                "punktfunk://connect/00000000-0000-4000-8000-000000000000?host=192.168.1.50"),
            .confirm(desk))
        // Nothing local matches: the sheet gets the address, the claimed name and the pin — which
        // is what makes a first connect verified rather than blind trust-on-first-use.
        XCTAssertEqual(
            try resolve("punktfunk://connect/10.0.0.9:7000?name=Studio"),
            .unknown(address: "10.0.0.9", port: 7000, name: "Studio", fp: nil))
        // But a stale record id is not a hostname: without `host=` there is nothing to dial.
        XCTAssertEqual(
            try resolve("punktfunk://connect/00000000-0000-4000-8000-000000000000"), .unresolvable)
        XCTAssertEqual(try resolve("punktfunk://connect/Basement%20PC"), .unresolvable)

        // A pin that contradicts the stored one is the link lying — the caller hard-refuses.
        let lying = try DeepLink.parse("punktfunk://connect/desk?fp=\(String(repeating: "b", count: 64))")
        XCTAssertTrue(lying.pinConflict(with: desk))
        let honest = try DeepLink.parse("punktfunk://connect/desk?fp=\(String(repeating: "a", count: 64))")
        XCTAssertFalse(honest.pinConflict(with: desk))
        // No pin stored → nothing to contradict; the trust flow runs as usual.
        XCTAssertFalse(lying.pinConflict(with: couchA))
    }

    /// Both OS installs of a dual-boot box at one address: the link's `fp` picks its own record,
    /// and a pin nobody holds is a new host — the sheet, never the neighbour.
    func testDeepLinkPinPicksItsOwnOSAtASharedAddress() throws {
        let windows = StoredHost(
            name: "Desk (Windows)", address: "192.168.1.50",
            pinnedSHA256: Data(repeating: 0xAA, count: 32))
        let linux = StoredHost(
            name: "Desk (Linux)", address: "192.168.1.50",
            pinnedSHA256: Data(repeating: 0xBB, count: 32))
        func resolve(_ fp: String) throws -> DeepLink.HostResolution {
            try DeepLink.parse("punktfunk://connect/192.168.1.50:9777?fp=\(fp)")
                .resolveHost(in: [windows, linux])
        }
        XCTAssertEqual(try resolve(String(repeating: "a", count: 64)), .confirm(windows))
        XCTAssertEqual(try resolve(String(repeating: "b", count: 64)), .confirm(linux))
        let third = String(repeating: "c", count: 64)
        XCTAssertEqual(
            try resolve(third), .unknown(address: "192.168.1.50", port: 9777, name: nil, fp: third))
    }

    // MARK: - Host grid arrangement

    private func arrangementFixture() -> (hosts: [StoredHost], catalog: PresetCatalog) {
        let catalog = PresetCatalog(presets: [
            StreamPreset(name: "Game", id: "111111111111", accent: "#ff8800"),
            StreamPreset(name: "Work", id: "222222222222"),
        ])
        // Stored order is the order they were added. Basement has no `addedAt` at all — it was
        // saved before the field existed, which is why it sits at the FRONT of the store: undated
        // hosts are always the oldest ones, never scattered through the middle.
        let basement = StoredHost(name: "Basement", address: "10.0.0.3")
        var desk = StoredHost(name: "Desk", address: "10.0.0.1",
                              addedAt: Date(timeIntervalSince1970: 100))
        desk.presetID = "111111111111"
        desk.lastConnected = Date(timeIntervalSince1970: 5_000)
        var attic = StoredHost(name: "attic", address: "10.0.0.2",
                               addedAt: Date(timeIntervalSince1970: 200))
        attic.presetID = "222222222222"
        var couch = StoredHost(name: "Couch", address: "10.0.0.4",
                               addedAt: Date(timeIntervalSince1970: 300))
        couch.lastConnected = Date(timeIntervalSince1970: 9_000)
        // Couch is bound to nothing but has PINNED both presets as their own cards.
        couch.pinnedPresetIDs = ["111111111111", "222222222222"]
        return ([basement, desk, attic, couch], catalog)
    }

    private func arranged(
        _ sort: HostSort, _ grouping: HostGrouping = .none, online: Set<UUID> = []
    ) -> [HostGroup] {
        let (hosts, catalog) = arrangementFixture()
        return HostArrangement.groups(
            hosts: hosts, catalog: catalog, online: online, sort: sort, grouping: grouping)
    }

    /// Card labels: the host, plus the pinned preset when the card carries one.
    private func labels(_ group: HostGroup) -> [String] {
        group.cards.map { card in
            card.pinned.map { "\(card.host.name)·\($0.name)" } ?? card.host.name
        }
    }

    /// The default is the order the grid had before it could sort at all — an update must not
    /// rearrange anyone's hosts. Each host is followed by its own pinned cards.
    ///
    /// Undated hosts sort FIRST, because having no date means predating the field means being
    /// older. In a real store they are a prefix (everything saved before the upgrade), so the
    /// result is the stored order exactly — which is the point.
    func testHostSortByDateAddedKeepsTheStoredOrder() {
        let group = arranged(.added)[0]
        XCTAssertNil(group.title, "ungrouped draws no header")
        XCTAssertEqual(
            labels(group), ["Basement", "Desk", "attic", "Couch", "Couch·Game", "Couch·Work"])
    }

    /// Case- and locale-insensitive, so "attic" doesn't sort after "Desk" the way a raw `<` on
    /// Strings would put every lowercase name below every uppercase one.
    func testHostSortByNameIgnoresCase() {
        XCTAssertEqual(
            arranged(.name)[0].cards.map(\.host.name),
            ["attic", "Basement", "Couch", "Couch", "Couch", "Desk"])
    }

    /// Most recent first — and a host you have NEVER connected to goes last, not first. Treating
    /// "never" as `.distantPast` would sort it as if it had been connected in 1970.
    func testHostSortByLastConnectedPutsNeverConnectedLast() {
        let names = arranged(.lastConnected)[0].cards.map(\.host.name)
        XCTAssertEqual(names.prefix(4).map { $0 }, ["Couch", "Couch", "Couch", "Desk"])
        // The two never-connected ones tie, so they keep their stored order — `sorted` is not
        // stable in Swift, and equal rows swapping between redraws is a visible bug.
        XCTAssertEqual(names.suffix(2).map { $0 }, ["Basement", "attic"])
    }

    /// ⭐ The regression this was written for: a PINNED card belongs to the preset it connects
    /// with, not to whatever its host is bound to. Grouping on the binding alone filed every
    /// pinned card under "No Preset" — including the ones visibly wearing a preset chip.
    func testHostGroupingFilesPinnedCardsUnderTheirOwnPreset() {
        let groups = arranged(.name, .preset)
        XCTAssertEqual(groups.map(\.title), ["Game", "Work", "No Preset"])
        // Desk is BOUND to Game; Couch merely pinned it. Both belong in the Game band.
        XCTAssertEqual(labels(groups[0]), ["Couch·Game", "Desk"])
        XCTAssertEqual(labels(groups[1]), ["attic", "Couch·Work"])
        // Couch's OWN card streams with the defaults, so that one is unbound.
        XCTAssertEqual(labels(groups[2]), ["Basement", "Couch"])
        XCTAssertEqual(groups[0].accent, "#ff8800", "the header matches its cards")
    }

    /// A preset nobody uses gets no band at all.
    func testHostGroupingSkipsEmptyBands() {
        let (hosts, _) = arrangementFixture()
        let unused = PresetCatalog(presets: [StreamPreset(name: "Travel", id: "333333333333")])
        let groups = HostArrangement.groups(
            hosts: hosts, catalog: unused, online: [], sort: .name, grouping: .preset)
        XCTAssertEqual(groups.map(\.title), ["No Preset"])
        // The pins point at presets this catalog doesn't have, so they produce no cards either.
        XCTAssertEqual(labels(groups[0]), ["attic", "Basement", "Couch", "Desk"])
    }

    /// A dangling binding resolves as no preset (§4.4), so it lands in the unbound band rather
    /// than in one named after a preset that no longer exists.
    func testHostGroupingDropsDanglingBindings() {
        var host = StoredHost(name: "Desk", address: "10.0.0.1")
        host.presetID = "deadbeefdead"
        let groups = HostArrangement.groups(
            hosts: [host], catalog: PresetCatalog(), online: [], sort: .name, grouping: .preset)
        XCTAssertEqual(groups.map(\.title), ["No Preset"])
    }

    func testHostGroupingByStatus() {
        // One fixture, reused: each call mints fresh UUIDs, so an `online` set taken from a
        // second fixture would name hosts this one has never heard of.
        let (hosts, catalog) = arrangementFixture()
        func groups(online: Set<UUID>) -> [HostGroup] {
            HostArrangement.groups(
                hosts: hosts, catalog: catalog, online: online, sort: .name, grouping: .status)
        }

        // By name, not by index: the fixture's order is a fact about `.added`, not about this.
        let up = hosts.filter { ["Desk", "Couch"].contains($0.name) }.map(\.id)
        let some = groups(online: Set(up))
        XCTAssertEqual(some.map(\.title), ["Online", "Offline"])
        // A pinned card follows its host's status — same record, same reachability.
        XCTAssertEqual(labels(some[0]), ["Couch", "Couch·Game", "Couch·Work", "Desk"])
        XCTAssertEqual(labels(some[1]), ["attic", "Basement"])

        // All offline: no empty "Online" band.
        XCTAssertEqual(groups(online: []).map(\.title), ["Offline"])
        XCTAssertEqual(groups(online: Set(hosts.map(\.id))).map(\.title), ["Online"])
    }

    // MARK: - Settings presets

    /// The overlay applies field by field: a value wins, an absent one keeps the base's live
    /// value — including a value that happens to equal the base (an explicit pin).
    func testOverlayAppliesOnlyWhatItOverrides() {
        var base = EffectiveSettings()
        base.width = 1920
        base.height = 1080
        base.bitrateKbps = 20_000
        base.codec = "hevc"

        XCTAssertTrue(SettingsOverlay().isEmpty)
        XCTAssertEqual(base.applying(SettingsOverlay()), base)

        var overlay = SettingsOverlay()
        overlay.width = 3840
        overlay.height = 2160
        overlay.refreshHz = 120
        overlay.codec = "av1"
        overlay.enable444 = true
        overlay.tenBitSdr = true
        overlay.modifierLayout = "windows"
        XCTAssertFalse(overlay.isEmpty)
        let out = base.applying(overlay)
        XCTAssertEqual([out.width, out.height, out.refreshHz], [3840, 2160, 120])
        XCTAssertEqual(out.codec, "av1")
        XCTAssertTrue(out.enable444)
        XCTAssertTrue(out.tenBitSdr)
        XCTAssertEqual(out.modifierLayout, "windows")
        // Untouched fields keep following the base.
        XCTAssertEqual(out.bitrateKbps, 20_000)
        // Tier-G endpoints are not in the overlay at all — no preset can move this device's
        // speaker or microphone.
        XCTAssertEqual(out.speakerUID, base.speakerUID)

        // An overlay carrying a value equal to the base is still an override: the preset PINS it,
        // so a later change to the global doesn't move it.
        var pin = SettingsOverlay()
        pin.bitrateKbps = 20_000
        XCTAssertFalse(pin.isEmpty)
        var moved = base
        moved.bitrateKbps = 50_000
        XCTAssertEqual(moved.applying(pin).bitrateKbps, 20_000)
    }

    /// `clear` is the explicit way back to inheriting, including the resolution tri-state one row
    /// drives.
    func testOverlayClearDropsOneOverride() {
        var overlay = SettingsOverlay()
        overlay.width = 3840
        overlay.height = 2160
        overlay.matchWindow = false
        overlay.codec = "av1"
        XCTAssertTrue(OverlayField.isOverridden("resolution", in: overlay))
        XCTAssertTrue(OverlayField.clear("codec", in: &overlay))
        XCTAssertNil(overlay.codec)
        XCTAssertTrue(OverlayField.clear(OverlayField.resolution, in: &overlay))
        XCTAssertNil(overlay.width)
        XCTAssertNil(overlay.height)
        XCTAssertNil(overlay.matchWindow)
        XCTAssertTrue(overlay.isEmpty)
        XCTAssertFalse(OverlayField.clear("no_such_field", in: &overlay))
    }

    /// A catalog round-trips, and what this build can't represent survives it: an unknown overlay
    /// KEY is carried through untouched rather than erased — the don't-clobber rule, which is what
    /// keeps an older build from silently gutting a preset a newer one wrote.
    func testCatalogRoundTripsAndPreservesUnknownKeys() throws {
        let stored = """
        {
          "version": 1,
          "presets": [
            {
              "id": "a1b2c3d4e5f6", "name": "Game", "accent": "#ff8800",
              "overrides": {
                "width": 3840, "height": 2160, "refresh_hz": 120,
                "codec": "vvc-from-the-future", "stats_verbosity": "compact",
                "some_new_axis": {"nested": true}
              },
              "future_preset_key": 7
            },
            { "id": "0f0f0f0f0f0f", "name": "Work" }
          ]
        }
        """.data(using: .utf8)!

        let catalog = try JSONDecoder().decode(PresetCatalog.self, from: stored)
        XCTAssertEqual(catalog.presets.count, 2)
        let game = try XCTUnwrap(catalog.preset(id: "a1b2c3d4e5f6"))
        XCTAssertEqual(game.accent, "#ff8800")
        XCTAssertEqual(game.overrides.codec, "vvc-from-the-future")
        XCTAssertEqual(game.overrides.statsVerbosity, "compact")
        // A preset with no `overrides` key at all is the empty (inherit-everything) one.
        XCTAssertTrue(try XCTUnwrap(catalog.preset(id: "0f0f0f0f0f0f")).overrides.isEmpty)

        let text = try XCTUnwrap(String(data: JSONEncoder().encode(catalog), encoding: .utf8))
        XCTAssertTrue(text.contains("some_new_axis"))
        XCTAssertTrue(text.contains("future_preset_key"))
        // Absent overrides serialize away entirely — "not overridden" has one representation.
        XCTAssertFalse(text.contains("null"))
        let round = try JSONDecoder().decode(PresetCatalog.self, from: Data(text.utf8))
        XCTAssertEqual(round.preset(id: "a1b2c3d4e5f6")?.overrides.width, 3840)
        XCTAssertEqual(round.preset(id: "a1b2c3d4e5f6")?.overrides.extra.count, 1)
        XCTAssertEqual(round.preset(id: "a1b2c3d4e5f6")?.extra.count, 1)
    }

    /// A host saved before the rename keeps its bindings, and encoding writes both spellings so an
    /// older build reads them too. Both present: the new one wins, since only this build writes it.
    func testStoredHostReadsAndMirrorsPreRenameKeys() throws {
        let json = """
        {"id":"11111111-2222-3333-4444-555555555555","name":"Old","address":"10.0.0.5",
         "port":9777,"profileID":"a1b2c3d4e5f6","pinnedProfileIDs":["0f0f0f0f0f0f"]}
        """.data(using: .utf8)!
        let host = try JSONDecoder().decode(StoredHost.self, from: json)
        XCTAssertEqual(host.presetID, "a1b2c3d4e5f6")
        XCTAssertEqual(host.pinnedPresetIDs, ["0f0f0f0f0f0f"])
        let saved = try XCTUnwrap(
            JSONSerialization.jsonObject(with: JSONEncoder().encode(host)) as? [String: Any])
        XCTAssertEqual(saved["presetID"] as? String, "a1b2c3d4e5f6")
        XCTAssertEqual(saved["profileID"] as? String, "a1b2c3d4e5f6")
        XCTAssertEqual(saved["pinnedProfileIDs"] as? [String], ["0f0f0f0f0f0f"])

        let both = """
        {"id":"11111111-2222-3333-4444-555555555555","name":"Old","address":"10.0.0.5",
         "port":9777,"presetID":"111111111111","profileID":"222222222222"}
        """.data(using: .utf8)!
        XCTAssertEqual(try JSONDecoder().decode(StoredHost.self, from: both).presetID, "111111111111")
    }

    /// A catalog saved before the rename loads from its old key and its old defaults entry, an
    /// emptied catalog does not fall back to it, and a stored `profile` grouping still groups.
    func testPreRenameCatalogAndGroupingStillLoad() throws {
        let suite = "preset-rename-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let old = #"{"version": 1, "profiles": [{"id": "a1b2c3d4e5f6", "name": "Game"}]}"#
        defaults.set(Data(old.utf8), forKey: DefaultsKey.legacyPresets)
        XCTAssertEqual(PresetCatalog.load(from: defaults).presets.map(\.name), ["Game"])
        PresetCatalog(presets: []).save(to: defaults)
        XCTAssertTrue(PresetCatalog.load(from: defaults).presets.isEmpty)
        XCTAssertEqual(HostGrouping(rawValue: "profile"), .preset)
        XCTAssertEqual(HostGrouping.preset.rawValue, "preset")
    }

    /// Reference resolution: id first, then a unique case-insensitive name; two presets sharing a
    /// name resolve to `.ambiguous` (the caller refuses) rather than to whichever came first.
    func testCatalogResolvesIDsFirstAndRefusesAmbiguity() {
        let catalog = PresetCatalog(presets: [
            StreamPreset(name: "Work", id: "111111111111"),
            StreamPreset(name: "work", id: "222222222222"),
            StreamPreset(name: "Game", id: "333333333333"),
        ])
        XCTAssertEqual(catalog.resolve("111111111111").1, .found)
        XCTAssertEqual(catalog.resolve("Work").1, .ambiguous)
        XCTAssertEqual(catalog.resolve("game").1, .found)
        XCTAssertEqual(catalog.resolve("GAME").0?.id, "333333333333")
        XCTAssertEqual(catalog.resolve("nope").1, .notFound)

        XCTAssertTrue(catalog.nameTaken("GAME"))
        XCTAssertFalse(catalog.nameTaken("GAME", except: "333333333333"))
        XCTAssertTrue(catalog.nameTaken("GAME", except: "111111111111"))
        XCTAssertFalse(catalog.nameTaken("Travel"))
    }

    /// Bindings and pins live ON the host record, and both degrade rather than error: a deleted
    /// preset means "Default settings" for a binding and a vanished card for a pin.
    func testBindingsAndPinsDropDanglingIDs() {
        let catalog = PresetCatalog(presets: [
            StreamPreset(name: "Game", id: "111111111111"),
            StreamPreset(name: "Work", id: "222222222222"),
        ])
        var host = StoredHost(name: "Desk", address: "10.0.0.1")
        XCTAssertNil(catalog.binding(for: host))

        host.presetID = "111111111111"
        XCTAssertEqual(catalog.binding(for: host)?.name, "Game")
        host.presetID = "deadbeefdead"
        XCTAssertNil(catalog.binding(for: host), "a deleted preset is Default settings, not an error")

        host.pinnedPresetIDs = ["222222222222", "deadbeefdead", "222222222222", "111111111111"]
        XCTAssertEqual(catalog.pinned(for: host).map(\.id), ["222222222222", "111111111111"])
    }

    /// A title's own binding beats the host's default for that launch only, and a deleted one
    /// falls through to the default rather than past it to the globals.
    func testATitleBindingBeatsTheHostDefault() {
        let catalog = PresetCatalog(presets: [
            StreamPreset(name: "Game", id: "111111111111"),
            StreamPreset(name: "Work", id: "222222222222"),
        ])
        var host = StoredHost(name: "Desk", address: "10.0.0.1")
        host.presetID = "111111111111"
        host.gamePresets = ["halo": "222222222222", "gone": "deadbeefdead"]

        XCTAssertEqual(catalog.binding(for: host, launch: "halo")?.name, "Work")
        XCTAssertEqual(catalog.binding(for: host, launch: "other")?.name, "Game")
        XCTAssertEqual(catalog.binding(for: host, launch: "gone")?.name, "Game")
        XCTAssertEqual(catalog.binding(for: host)?.name, "Game", "the desktop keeps the default")

        // A one-off pick (a pinned card) still wins over the title's binding.
        let pinned = EffectiveSettings.resolve(
            host: host, selection: .preset("111111111111"), launch: "halo", catalog: catalog)
        XCTAssertEqual(pinned.presetName, "Game")
        let plain = EffectiveSettings.resolve(host: host, launch: "halo", catalog: catalog)
        XCTAssertEqual(plain.presetName, "Work")
    }

    /// The accent is a plain `#RRGGBB` string in the catalog — the palette is what this client
    /// OFFERS, not what it accepts, so a colour another platform wrote still renders and a
    /// malformed one falls back to the brand tint rather than to black.
    func testPresetAccentParsing() {
        XCTAssertNotNil(Color(hex: "#ff8800"))
        XCTAssertNil(Color(hex: "ff8800"), "a missing # is not a colour")
        XCTAssertNil(Color(hex: "#ff88"), "a short value is not a colour")
        XCTAssertNil(Color(hex: "#gggggg"), "non-hex is not a colour")
        XCTAssertNil(Color(hex: ""))

        // Every offered swatch parses, and each is distinguishable by name and value.
        XCTAssertEqual(Set(PresetAccent.palette.map(\.hex)).count, PresetAccent.palette.count)
        for accent in PresetAccent.palette {
            XCTAssertNotNil(Color(hex: accent.hex), accent.name)
            XCTAssertEqual(PresetAccent.named(accent.hex.uppercased())?.name, accent.name)
        }
        XCTAssertNil(PresetAccent.named(nil))
        XCTAssertNil(PresetAccent.named("#123456"), "an unlisted colour has no palette name")
    }

    /// The whole per-connect resolution, in the precedence every client shares:
    /// one-off pick ?? host binding ?? none.
    func testEffectiveSettingsResolutionPrecedence() {
        let defaults = UserDefaults(suiteName: "io.unom.punktfunk.tests.effective")!
        defaults.removePersistentDomain(forName: "io.unom.punktfunk.tests.effective")
        defaults.set(2_560, forKey: DefaultsKey.streamWidth)
        defaults.set(30_000, forKey: DefaultsKey.bitrateKbps)

        var gameOverrides = SettingsOverlay()
        gameOverrides.bitrateKbps = 80_000
        var workOverrides = SettingsOverlay()
        workOverrides.bitrateKbps = 8_000
        let catalog = PresetCatalog(presets: [
            StreamPreset(
                name: "Game", id: "111111111111", accent: "#ff8800", overrides: gameOverrides),
            StreamPreset(name: "Work", id: "222222222222", overrides: workOverrides),
        ])
        var host = StoredHost(name: "Desk", address: "10.0.0.1")
        host.presetID = "111111111111"

        let unbound = EffectiveSettings.resolve(
            host: StoredHost(name: "Plain", address: "10.0.0.2"),
            catalog: catalog, defaults: defaults)
        XCTAssertEqual(unbound.bitrateKbps, 30_000)
        XCTAssertNil(unbound.presetID)
        XCTAssertEqual(unbound.width, 2_560, "globals still flow through in every case")

        let bound = EffectiveSettings.resolve(host: host, catalog: catalog, defaults: defaults)
        XCTAssertEqual(bound.bitrateKbps, 80_000)
        XCTAssertEqual(bound.presetName, "Game")
        // The chip colour rides along, so the HUD can name the session in the same colour the
        // card that launched it wore.
        XCTAssertEqual(bound.presetAccent, "#ff8800")

        // A one-off pick wins over the binding — and does not rebind anything.
        let oneOff = EffectiveSettings.resolve(
            host: host, selection: .preset("222222222222"),
            catalog: catalog, defaults: defaults)
        XCTAssertEqual(oneOff.bitrateKbps, 8_000)
        XCTAssertEqual(host.presetID, "111111111111")

        // "Connect with ▸ Default settings" on a BOUND host forces the globals — the case that
        // makes this a three-way selection rather than an optional preset.
        XCTAssertEqual(
            EffectiveSettings.resolve(host: host, selection: .defaults, catalog: catalog,
                                      defaults: defaults).bitrateKbps,
            30_000)
        // A one-off naming a preset that no longer exists degrades to the globals rather than
        // erroring — same rule as a dangling binding.
        XCTAssertEqual(
            EffectiveSettings.resolve(host: host, selection: .preset("gone"), catalog: catalog,
                                      defaults: defaults).bitrateKbps,
            30_000)

        defaults.removePersistentDomain(forName: "io.unom.punktfunk.tests.effective")
    }

    /// The console's Native is a zero; a host refuses a 0 Hz or 0 px mode, so it never goes out.
    func testStreamModeResolvesNativeZeros() {
        var s = EffectiveSettings()
        (s.width, s.height, s.refreshHz) = (0, 0, 0)
        let native = s.streamMode(native: (width: 3_840, height: 2_160, hz: 60))
        XCTAssertEqual([native.width, native.height, native.hz], [3_840, 2_160, 60])
        XCTAssertEqual(s.streamMode(native: (width: 3_840, height: 2_160, hz: 0)).hz, 30)
        (s.width, s.height, s.refreshHz) = (1_920, 1_080, 120)
        let set = s.streamMode(native: (width: 3_840, height: 2_160, hz: 60))
        XCTAssertEqual([set.width, set.height, set.hz], [1_920, 1_080, 120])
    }

    // MARK: - The audio format a preset carries

    /// `AudioFormatChoice`'s raw values are a CROSS-CLIENT contract, not an implementation detail:
    /// they are what a preset stores, and the desktop clients (`pf_client_core::session::
    /// AUDIO_FORMATS`) and Android (`Settings.kt`'s `AUDIO_FORMAT_*`) key the same table off the
    /// same strings, so one preset catalog has to round-trip through all four. Renaming one fails
    /// in the worst possible way: the key is carried through untouched, so the preset keeps
    /// "working" on the other client and silently inherits its global default — the setting does
    /// not error, the session just quietly costs less and sounds worse.
    ///
    /// So every value is frozen here character by character, including the naming rule the 44.1 kHz
    /// family follows: the kHz figure with the decimal point dropped.
    func testAudioFormatRawValuesAreTheCrossClientContract() {
        XCTAssertEqual(AudioFormatChoice.opus.rawValue, "opus")
        XCTAssertEqual(AudioFormatChoice.lossless48.rawValue, "lossless48")
        XCTAssertEqual(AudioFormatChoice.lossless96.rawValue, "lossless96")
        XCTAssertEqual(AudioFormatChoice.lossless441.rawValue, "lossless441")
        XCTAssertEqual(AudioFormatChoice.lossless882.rawValue, "lossless882")
        XCTAssertEqual(AudioFormatChoice.lossless1764.rawValue, "lossless1764")

        // Ordered by rate, which is the order the settings row lists them in — and the same order
        // as the Android and desktop tables, so the three menus read alike. Pinned as a whole so a
        // case added here is a case somebody had to look at `SettingsOptions.audioFormats` for:
        // that table is in the app target and cannot be reached from these tests.
        XCTAssertEqual(
            AudioFormatChoice.allCases.map(\.rawValue),
            ["opus", "lossless441", "lossless48", "lossless882", "lossless96", "lossless1764"])
    }

    /// Every lossless row must reach the wire as the rate it names, at 24-bit — and `opus` must
    /// stay exactly `48 000`/`16`, which is byte-for-byte a pre-lossless request and is what keeps
    /// the default session on the legacy connect path.
    ///
    /// The five rates are `punktfunk_core::audio::pcm::rate_is_supported`. 44.1/88.2/176.4 were
    /// absent until the jitter policy stopped dividing by 1 000 before it multiplied
    /// (design/hi-res-audio.md §4.1); nothing else ever blocked them.
    func testAudioFormatWireMappingCoversBothRateFamilies() {
        XCTAssertEqual(AudioFormatChoice.opus.wire.rateHz, 48_000)
        XCTAssertEqual(AudioFormatChoice.opus.wire.bits, 16)
        XCTAssertFalse(AudioFormatChoice.opus.isLossless)

        let expected: [(AudioFormatChoice, UInt32)] = [
            (.lossless441, 44_100), (.lossless48, 48_000), (.lossless882, 88_200),
            (.lossless96, 96_000), (.lossless1764, 176_400),
        ]
        for (choice, rateHz) in expected {
            XCTAssertEqual(choice.wire.rateHz, rateHz, "\(choice.rawValue)")
            XCTAssertEqual(choice.wire.bits, 24, "\(choice.rawValue): 24-bit earns the bandwidth")
            XCTAssertTrue(choice.isLossless, "\(choice.rawValue)")
        }
        XCTAssertEqual(
            expected.count + 1, AudioFormatChoice.allCases.count,
            "a case with no wire pair here would connect at whatever the switch fell through to")
    }

    /// A stored value this build does not know — a newer build's, or a corrupted pref — resolves to
    /// Opus rather than blocking the connect, which is what the desktop `audio_format_wire` and the
    /// Android `audioFormatWire` do with the same string.
    func testUnknownAudioFormatFallsBackToOpus() {
        for stored in ["", "lossless192", "lossless44", "lossless44_1", "LOSSLESS48", "flac"] {
            XCTAssertEqual(
                AudioFormatChoice(setting: stored), .opus,
                "\(stored) must not block the connect")
        }
        XCTAssertEqual(AudioFormatChoice(setting: "lossless441"), .lossless441)
    }
}
