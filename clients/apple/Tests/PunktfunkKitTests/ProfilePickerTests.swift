// The profile picker's rule, replayed from `clients/shared/profile-picker-vectors.json` (the
// vectors `crates/client/pf-client-core/src/profiles.rs` runs too), plus how a host row decodes.

import XCTest
@testable import PunktfunkShared

final class ProfilePickerTests: XCTestCase {
    private struct Vectors: Decodable {
        struct Case: Decodable {
            struct Expect: Decodable {
                let picker: Bool
                let send: String?
                let gone: String?
                let remember: String?
            }
            let name: String
            let listed: [ListedProfile]?
            let remembered: ProfilePick?
            let link: String?
            let expect: Expect
        }
        let cases: [Case]
    }

    func testPickerFollowsTheSharedVectors() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/profile-picker-vectors.json")
        let v = try JSONDecoder().decode(Vectors.self, from: Data(contentsOf: url))
        XCTAssertFalse(v.cases.isEmpty)
        for c in v.cases {
            let d = HostProfiles.pickerDecision(
                listed: c.listed, remembered: c.remembered, link: c.link)
            XCTAssertEqual(d.picker, c.expect.picker, "\(c.name): picker")
            XCTAssertEqual(d.send, c.expect.send, "\(c.name): send")
            XCTAssertEqual(d.gone, c.expect.gone, "\(c.name): gone")
            XCTAssertEqual(d.remember?.id, c.expect.remember, "\(c.name): remember")
        }
    }

    func testAHostRowDecodesAndWordsItsSeat() throws {
        let raw = """
            [{"id":"9a3f1c2b7e40","display_name":"Kid","accent":"#f97316","owner":false,
              "home":"bigpicture","future":1,
              "seat":{"state":"occupied","port":9777,"occupant":"Ben's Apple TV"}},
             {"id":"x","display_name":"Odd","seat":{"state":"sleeping","port":1}}]
            """
        let rows = try JSONDecoder().decode([ListedProfile].self, from: Data(raw.utf8))
        XCTAssertEqual(rows[0].note, "In use by Ben's Apple TV")
        XCTAssertEqual(rows[1].seat?.state, .other)
        XCTAssertNil(rows[1].note)
        XCTAssertEqual(HostProfiles.initials("anna lena x"), "AL")
    }

    func testASeatIsDialedWokenWaitedForOrRefused() {
        func row(_ state: SeatState?, _ detail: String? = nil) -> ListedProfile {
            ListedProfile(
                id: "kid", displayName: "Kid",
                seat: state.map { ProfileSeat(state: $0, detail: detail) })
        }
        XCTAssertEqual(HostProfiles.seatGate(row(nil)), .dial)
        XCTAssertEqual(HostProfiles.seatGate(row(.ready)), .dial)
        XCTAssertEqual(HostProfiles.seatGate(row(.occupied)), .dial)
        XCTAssertEqual(HostProfiles.seatGate(row(.other)), .dial)
        XCTAssertEqual(HostProfiles.seatGate(row(.stopped)), .wake)
        XCTAssertEqual(
            HostProfiles.seatGate(row(.starting, "Signing in")), .wait(detail: "Signing in"))
        XCTAssertEqual(HostProfiles.seatGate(row(.starting)), .wait(detail: nil))
        XCTAssertEqual(
            HostProfiles.seatGate(row(.unavailable, "Seats are off.")), .refuse("Seats are off."))
        XCTAssertEqual(
            HostProfiles.seatGate(row(.unavailable)),
            .refuse("That profile can't play on this host right now."))
        XCTAssertEqual(HostProfiles.wakingLine("Kid"), "Getting Kid's desk ready…")
    }

    func testAnOldSavedHostDecodesWithoutAPick() throws {
        let old = """
            {"id":"11111111-2222-4333-8444-555555555555","name":"Desk","address":"10.0.0.2",
             "port":9777,"profileID":"preset"}
            """
        let host = try JSONDecoder().decode(StoredHost.self, from: Data(old.utf8))
        XCTAssertNil(host.pickedProfile)
        XCTAssertEqual(host.presetID, "preset")
        var picked = host
        picked.pickedProfile = ProfilePick(id: "kid", displayName: "Kid")
        let back = try JSONDecoder().decode(
            StoredHost.self, from: try JSONEncoder().encode(picked))
        XCTAssertEqual(back.pickedProfile, picked.pickedProfile)
        XCTAssertEqual(back.presetID, "preset")
    }
}
