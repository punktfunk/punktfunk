// `/status` as an answer or a miss (D3): only a 200 with a status body is an answer.

import XCTest
@testable import PunktfunkKit

final class LibraryStatusAnswerTests: XCTestCase {
    private func response(_ status: Int, _ body: String) -> HTTPResponse {
        HTTPResponse(status: status, headers: [:], body: Data(body.utf8))
    }

    func testAStatusBodyIsAnAnswer() throws {
        let body = #"""
            {"games":[{"app_id":"steam:570","title":"Dota 2","state":"running","endable":true}],
             "downloads":[{"app_id":"steam:620","state":"downloading","done_bytes":5}],
             "grants":3,"sessions":[]}
            """#
        let answer = try XCTUnwrap(LibraryClient.statusAnswer(response(200, body)))
        XCTAssertEqual(answer.games.map(\.appID), ["steam:570"])
        XCTAssertEqual(answer.downloads.map(\.appID), ["steam:620"])
        XCTAssertEqual(answer.grants, 3)
    }

    func testAnEmptyStatusIsAnAnswerThatNothingRuns() throws {
        let answer = try XCTUnwrap(LibraryClient.statusAnswer(response(200, "{}")))
        XCTAssertTrue(answer.games.isEmpty)
        XCTAssertTrue(answer.downloads.isEmpty)
        XCTAssertNil(answer.grants)
    }

    func testNoResponseIsAMiss() {
        XCTAssertNil(LibraryClient.statusAnswer(nil))
    }

    func testARefusalIsAMiss() {
        XCTAssertNil(LibraryClient.statusAnswer(response(404, "{}")))
        XCTAssertNil(LibraryClient.statusAnswer(response(503, "{}")))
    }

    func testABodyThatIsNotAStatusIsAMiss() {
        XCTAssertNil(LibraryClient.statusAnswer(response(200, "<html>")))
        XCTAssertNil(LibraryClient.statusAnswer(response(200, #"{"games":"many"}"#)))
    }
}
