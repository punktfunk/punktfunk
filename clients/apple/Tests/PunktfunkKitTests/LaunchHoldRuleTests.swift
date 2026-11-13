// LaunchHoldRule.step: a row per state the launch watch reads off the host's status.

import Foundation
import XCTest

@testable import PunktfunkKit

final class LaunchHoldRuleTests: XCTestCase {
    private let cap: TimeInterval = 120
    private let noLease: TimeInterval = 15
    private let files = HostDownload(appID: "steam:570", state: "downloading", doneBytes: 10)

    private func step(
        _ state: String?, window: Bool = false, download: LaunchHoldRule.Download? = nil,
        after elapsed: TimeInterval = 0
    ) -> LaunchHoldRule {
        LaunchHoldRule.step(
            LaunchHoldRule.Title(state: state, awaitingWindow: window, download: download),
            elapsed: elapsed, cap: cap, noLease: noLease)
    }

    private let starting = LaunchHoldRule.keep(windowWait: false, download: nil)

    /// A title the host still starts holds up to the cap.
    func testLaunchingHoldsUpToTheCap() {
        XCTAssertEqual(step("launching", after: 119), starting)
        XCTAssertEqual(step("launching", after: 120), .reveal)
    }

    /// A running game whose window the host awaits holds, saying so, up to the same cap.
    func testARunningGameAwaitingItsWindowHoldsUpToTheCap() {
        XCTAssertEqual(step("running", window: true, after: 119), .keep(windowWait: true, download: nil))
        XCTAssertEqual(step("running", window: true, after: 120), .reveal)
    }

    /// Every other state is all the host will say: the hold lifts at once.
    func testEveryOtherStateRevealsAtOnce() {
        for state in ["running", "window", "exited", "untracked", "grace", "detached", "new"] {
            XCTAssertEqual(step(state), .reveal, state)
        }
        // A window wait only counts on a running game.
        XCTAssertEqual(step("window", window: true), .reveal)
    }

    /// A title the host never lists holds only the short cap.
    func testNoLeaseHoldsUpToItsOwnCap() {
        XCTAssertEqual(step(nil, after: 14), starting)
        XCTAssertEqual(step(nil, after: 15), .reveal)
    }

    /// D3: a read the host didn't answer is not an answer that lists nothing. The hold, what it
    /// shows and its clock stand; only the long cap still lifts it.
    func testAMissIsNotAnEmptyAnswer() {
        let miss = { LaunchHoldRule.step(nil, elapsed: $0, cap: self.cap, noLease: self.noLease) }
        XCTAssertEqual(miss(15), .unchanged)
        XCTAssertEqual(miss(119), .unchanged)
        XCTAssertFalse(miss(15).restartsClock)
        XCTAssertEqual(miss(120), .reveal)
    }

    /// The files coming in hold past every cap, show on the hold, and start the clock over.
    func testALiveDownloadHoldsAndRestartsTheClock() {
        for state in [nil, "launching"] {
            let held = step(state, download: .live(files), after: 600)
            XCTAssertEqual(held, .keep(windowWait: false, download: files))
            XCTAssertTrue(held.restartsClock)
        }
        XCTAssertFalse(step("launching").restartsClock)
        XCTAssertFalse(LaunchHoldRule.reveal.restartsClock)
    }

    /// A download that stopped starts nothing: the hold lifts unless the game runs anyway.
    func testAStoppedDownloadRevealsATitleThatHasNotStarted() {
        XCTAssertEqual(step(nil, download: .stopped), .reveal)
        XCTAssertEqual(step("launching", download: .stopped), .reveal)
        XCTAssertEqual(
            step("running", window: true, download: .stopped), .keep(windowWait: true, download: nil))
    }

    /// The facts come from the held title's own row and download, by id.
    func testTheTitleReadsItsOwnRowAndDownload() throws {
        let entry = GameEntry(id: "steam:570", store: "steam", title: "Dota 2", art: Artwork())
        let games = try JSONDecoder().decode([RunningGame].self, from: Data("""
            [{"app_id":"steam:1","title":"Other","state":"launching"},
             {"app_id":"steam:570","title":"Dota 2","state":"running","awaiting_window":true}]
            """.utf8))
        let other = HostDownload(appID: "steam:1", state: "downloading")
        XCTAssertEqual(
            LaunchHoldRule.Title(of: entry, games: games, downloads: [other, files]),
            LaunchHoldRule.Title(state: "running", awaitingWindow: true, download: .live(files)))
        XCTAssertEqual(
            LaunchHoldRule.Title(of: entry, games: [], downloads: [other]),
            LaunchHoldRule.Title(state: nil))
        for state in ["queued", "installing"] {
            let live = HostDownload(appID: "steam:570", state: state)
            XCTAssertEqual(
                LaunchHoldRule.Title(of: entry, games: [], downloads: [live]).download,
                .live(live), state)
        }
        for state in ["failed", "cancelled", "paused"] {
            let stopped = HostDownload(appID: "steam:570", state: state)
            XCTAssertEqual(
                LaunchHoldRule.Title(of: entry, games: [], downloads: [stopped]).download,
                .stopped, state)
        }
        // A finished download is no download.
        let done = HostDownload(appID: "steam:570", state: "done")
        XCTAssertNil(LaunchHoldRule.Title(of: entry, games: [], downloads: [done]).download)
    }
}
