// The host's sentences worded by id, replayed from `clients/shared/host-text-vectors.json` (the
// vectors `crates/client/pf-client-core/src/host_text.rs` and the Kotlin `HostTextTest` run too).

import PunktfunkKit
import XCTest
@testable import PunktfunkShared

final class HostTextTests: XCTestCase {
    /// Every case words as its `sentence`. A message the build has reads in the app's language,
    /// not the host's English beside it; an id it does not know, or one that is not the host's to
    /// ask for, reads as that English.
    func testEveryVectorWordsAsItsSentence() throws {
        let file = try SharedVector.load("host-text-vectors.json")
        let cases = try XCTUnwrap(file["cases"] as? [[String: Any]])
        XCTAssertGreaterThan(cases.count, 50)
        for c in cases {
            let id = try XCTUnwrap(c["id"] as? String)
            let params = try XCTUnwrap(c["params"] as? [String: String])
            let english = try XCTUnwrap(c["english"] as? String)
            let sentence = try XCTUnwrap(c["sentence"] as? String)
            XCTAssertEqual(HostText.word(id, params: params, english: english), sentence, id)
        }
    }

    /// A newer host may send more values than this build's message holds, or fewer.
    func testValuesThatDoNotFitTheMessageReadAsTheHostsEnglish() {
        XCTAssertEqual(
            HostText.word(code: "credential_wrong", params: [:], english: "That isn't their password."),
            "That isn't their password.")
        XCTAssertEqual(
            HostText.word(
                code: "credential_wrong", params: ["account": "jdoe", "extra": "x"],
                english: "That isn't jdoe's password."),
            "That isn't jdoe's password.")
        XCTAssertEqual(
            HostText.word("launch_closed_at_once", params: ["title": "Quail"], english: "Closed."),
            "Closed.")
        XCTAssertEqual(
            HostText.word("apierr_profile_limit", params: [:], english: "Too many."), "Too many.")
    }

    func testAnApiErrorIsWordedByItsCode() {
        let coded = Data(
            #"{"error":"The host's English.","code":"title_in_use","params":{"title":"Quail"}}"#.utf8)
        XCTAssertEqual(HostText.apiError(coded), "Quit Quail first.")
        let newer = Data(#"{"error":"Quit Quail first.","code":"from_a_newer_host"}"#.utf8)
        XCTAssertEqual(HostText.apiError(newer), "Quit Quail first.")
        XCTAssertEqual(HostText.apiError(Data(#"{"error":"unknown setting id"}"#.utf8)), "unknown setting id")
        XCTAssertNil(HostText.apiError(Data(#"{"error":""}"#.utf8)))
        XCTAssertNil(HostText.apiError(Data()))
    }

    /// A seat line the host names is worded when the row decodes; the supervisor's own text and an
    /// id from a newer host stay the host's English.
    func testASeatLineIsWordedWhenItDecodes() throws {
        func seat(_ extra: String) throws -> ProfileSeat {
            let json = #"{"state":"starting","detail":"The host's English.",\#(extra)}"#
            return try JSONDecoder().decode(ProfileSeat.self, from: Data(json.utf8))
        }
        XCTAssertEqual(
            try seat(#""detail_id":"seatline_stage_sign_in_try","detail_params":{"attempt":"2","total":"3"}"#).detail,
            "Signing in to the seat's Windows account (try 2 of 3)…")
        XCTAssertEqual(
            try seat(#""detail_id":"seatline_from_a_newer_host""#).detail, "The host's English.")
        XCTAssertEqual(try seat(#""occupant":"Ben""#).detail, "The host's English.")
    }

    /// An action's reason is worded by its code; the system's own words arrive without one.
    func testAnActionReasonIsWordedByItsCode() throws {
        func reason(_ extra: String) throws -> String? {
            let json = """
                {"id":"power.sleep","title":"Sleep host","group":"power","danger":false,
                 "available":false,"permitted":true,\(extra)}
                """
            return try JSONDecoder().decode(HostAction.self, from: Data(json.utf8)).unavailableReason
        }
        XCTAssertEqual(
            try reason(#""unavailable_reason":"The host's English.","unavailable_code":"host_power_not_supported_here""#),
            "This machine does not support it")
        XCTAssertEqual(
            try reason(#""unavailable_reason":"The host's English.","unavailable_code":"from_a_newer_host""#),
            "The host's English.")
        XCTAssertEqual(
            try reason(#""unavailable_reason":"logind answered no""#), "logind answered no")
        XCTAssertEqual(HostAction(id: "host.restart", title: "x").label, "Restart Punktfunk")
        XCTAssertEqual(HostAction(id: "display.next", title: "x").label, "Next monitor")
    }

    /// The launch verdict crosses the C ABI as the id, the English and the placeholders.
    func testALaunchVerdictIsWordedFromItsLines() {
        XCTAssertEqual(
            HostText.launch(lines: "launch_closed_at_once\nThe host's English.\ntitle\tQuail\nseconds\t4"),
            "Couldn't start Quail — it closed again after 4 s.")
        XCTAssertEqual(HostText.launch(lines: "\nNothing to run."), "Nothing to run.")
        XCTAssertEqual(
            HostText.launch(lines: "launch_from_a_newer_host\nThe host's English."), "The host's English.")
        XCTAssertNil(HostText.launch(lines: ""))
    }
}
