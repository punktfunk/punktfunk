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

    /// The Rust client's rules: one row per title, only what the grants allow.
    func testATitlesMenuOffersTheOneActionItsFilesAllow() {
        let missing = TitleInstall(state: "missing", sizeBytes: 26_000_000_000, freeBytes: 212_000_000_000)
        var installed = missing
        installed.state = "installed"
        let all = PunktfunkConnection.grantAll
        let launch = PunktfunkConnection.grantLaunch
        XCTAssertNil(InstallAction.forTitle(nil, download: nil, grants: all))
        XCTAssertEqual(InstallAction.forTitle(missing, download: nil, grants: all), .install)
        XCTAssertEqual(
            InstallAction.forTitle(missing, download: HostDownload(appID: "a", state: "downloading"), grants: all),
            .pause)
        XCTAssertEqual(
            InstallAction.forTitle(missing, download: HostDownload(appID: "a", state: "paused"), grants: launch),
            .resume)
        XCTAssertEqual(InstallAction.forTitle(installed, download: nil, grants: all), .remove)
        XCTAssertNil(InstallAction.forTitle(installed, download: nil, grants: launch))
        XCTAssertNil(InstallAction.forTitle(installed, download: nil, grants: nil))
        XCTAssertEqual(InstallAction.install.label(missing), "Install \u{b7} 26 GB (212 GB free)")
        XCTAssertEqual(InstallAction.remove.label(installed), "Remove download \u{b7} 26 GB")

        XCTAssertEqual(TileBadge.forTitle(missing, download: nil), TileBadge(icon: "download", text: "26 GB"))
        XCTAssertNil(TileBadge.forTitle(installed, download: nil))
        XCTAssertEqual(
            TileBadge.forTitle(
                missing,
                download: HostDownload(appID: "a", state: "paused", doneBytes: 429, totalBytes: 1000)),
            TileBadge(icon: "pause", text: "42 %"))
        XCTAssertEqual(
            InstallOutcome.from(status: 409, message: "Quit Quail first.").notice(.remove, title: "Quail"),
            "Quit Quail first.")
        XCTAssertEqual(
            InstallOutcome.from(status: 404, message: nil).notice(.pause, title: "Quail"),
            "This host needs an update to manage games from here.")
    }

    func testTheConsoleBridgeNumbersDownloadsAsTheShellDoes() {
        XCTAssertEqual(
            ConsoleBridge.Push.libraryDownloads.rawValue,
            UInt8(PUNKTFUNK_CONSOLE_PUSH_LIBRARY_DOWNLOADS))
    }
}
