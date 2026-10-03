// A launch's download as its hold reads it; the words match the Rust and Kotlin shells'.

import PunktfunkCore
import XCTest
@testable import PunktfunkKit

final class HostDownloadTests: XCTestCase {
    func testStatusCarriesDownloadsBesideTheGames() throws {
        let json = """
        {"games":[{"app_id":"custom:a","title":"Quail","state":"launching"}],
         "downloads":[{"app_id":"custom:a","title":"Quail","state":"downloading",
         "done_bytes":12300000000,"total_bytes":26000000000,"rate_bps":48000000,"eta_s":240,
         "started_at":"x","updated_at":"y"}]}
        """
        let status = try JSONDecoder().decode(LibraryClient.HostStatus.self, from: Data(json.utf8))
        XCTAssertEqual(status.games?.first?.state, "launching")
        let d = try XCTUnwrap(status.downloads?.first)
        XCTAssertTrue(d.live)
        XCTAssertEqual(d.line, "12.3 GB of 26 GB \u{b7} 48 MB/s \u{b7} about 4 min left")
        XCTAssertEqual((d.fraction ?? 0) * 100, 47.3, accuracy: 0.1)
        XCTAssertNil(d.stopped(title: "Quail"))
        let old = try JSONDecoder().decode(LibraryClient.HostStatus.self, from: Data(#"{"games":[]}"#.utf8))
        XCTAssertNil(old.downloads)
    }

    func testAStoppedDownloadSaysWhyTheGameDidNotStart() {
        let failed = HostDownload(appID: "a", state: "failed", error: "the server is down")
        XCTAssertFalse(failed.live)
        XCTAssertEqual(failed.stopped(title: "Quail"), "Quail didn't download \u{2014} the server is down")
        XCTAssertEqual(
            HostDownload(appID: "a", state: "paused").stopped(title: "Quail"),
            "Quail's download was paused. Start it again to resume.")
        XCTAssertEqual(HostDownload(appID: "a", state: "installing", phase: "Verifying").line, "Verifying")
        XCTAssertEqual(HostDownload(appID: "a", state: "downloading", doneBytes: 3_100_000_000).line, "3.1 GB so far")
        XCTAssertEqual(HostDownload.bytes(512_000), "512 kB")
    }

    func testTheConsoleBridgeNumbersDownloadsAsTheShellDoes() {
        XCTAssertEqual(
            ConsoleBridge.Push.libraryDownloads.rawValue,
            UInt8(PUNKTFUNK_CONSOLE_PUSH_LIBRARY_DOWNLOADS))
    }
}
