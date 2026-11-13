// `/actions` and `/display/targets` as an answer or a miss (R3-5): a miss is never "nothing".

import XCTest

@testable import PunktfunkKit

final class HostAnswersTests: XCTestCase {
    private func response(_ status: Int, _ body: String) -> HTTPResponse {
        HTTPResponse(status: status, headers: [:], body: Data(body.utf8))
    }

    private static let actions = #"""
        {"actions":[
          {"id":"power.sleep","title":"Sleep","group":"power","danger":false,"available":true,
           "permitted":true},
          {"id":"power.reboot","title":"Restart","group":"power","danger":true,"available":true,
           "permitted":false}]}
        """#

    func testTheHostsPermittedActionsAreTheAnswer() throws {
        let answer = try XCTUnwrap(LibraryClient.actionsAnswer(response(200, Self.actions)))
        XCTAssertEqual(answer.map(\.id), ["power.sleep"])
        XCTAssertEqual(LibraryClient.actionsAnswer(response(200, #"{"actions":[]}"#)), [])
    }

    /// An older host without the route, or one that refuses this device, offers nothing.
    func testARefusalIsAnAnswerOfNothing() {
        for status in [401, 403, 404] {
            XCTAssertEqual(LibraryClient.actionsAnswer(response(status, "")), [], "\(status)")
            XCTAssertEqual(
                LibraryClient.targetsAnswer(response(status, "")), .some(nil), "\(status)")
        }
    }

    func testNoResponseIsAMiss() {
        XCTAssertNil(LibraryClient.actionsAnswer(nil))
        XCTAssertNil(LibraryClient.targetsAnswer(nil))
    }

    func testAFailingHostIsAMiss() {
        XCTAssertNil(LibraryClient.actionsAnswer(response(503, "")))
        XCTAssertNil(LibraryClient.targetsAnswer(response(500, "")))
    }

    func testABodyThatDoesNotReadIsAMiss() {
        XCTAssertNil(LibraryClient.actionsAnswer(response(200, "<html>")))
        XCTAssertNil(LibraryClient.targetsAnswer(response(200, #"{"targets":"many"}"#)))
    }

    func testTheHostsScreensAreTheAnswer() throws {
        let answer = try XCTUnwrap(LibraryClient.targetsAnswer(response(200, #"{"locked":true}"#)))
        XCTAssertEqual(answer?.locked, true)
    }

    /// A refusal the host words is its sentence alone, never an unreachable host.
    func testARefusedActionSaysTheHostsSentence() {
        let refusal = response(409, #"{"error":"another device is streaming from this host right now"}"#)
        XCTAssertThrowsError(try LibraryClient.expectAccepted(refusal)) {
            XCTAssertEqual(
                $0.localizedDescription, "another device is streaming from this host right now")
        }
        XCTAssertNoThrow(try LibraryClient.expectAccepted(response(202, "")))
        XCTAssertThrowsError(try LibraryClient.expectAccepted(response(403, #"{"error":"no"}"#))) {
            guard case LibraryError.unauthorized = $0 else { return XCTFail("\($0)") }
        }
    }
}
