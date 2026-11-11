// LinkArbiter: which window acts on a link or a Mac host page's request.

import XCTest

@testable import PunktfunkShared

final class LinkArbiterTests: XCTestCase {
    private func decide(
        _ kind: LinkArbiter.Kind, busy: Bool = false, front: Bool
    ) -> LinkArbiter.Decision {
        LinkArbiter.decide(kind: kind, isBusy: busy, isFront: front)
    }

    /// The key Mac window takes a link at once.
    func testTheKeyMacWindowTakesALinkAtOnce() {
        XCTAssertEqual(decide(.link(.mac), front: true), .runHere)
    }

    /// A Mac window behind it takes the link only if the key one did not.
    func testAMacWindowBehindWaitsForTheKeyOne() {
        XCTAssertEqual(decide(.link(.mac), front: false), .retryAfter(0.25))
    }

    /// A busy window still takes a link: `DeepLinkRouter` answers it there.
    func testABusyWindowStillTakesALink() {
        XCTAssertEqual(decide(.link(.mac), busy: true, front: true), .runHere)
    }

    /// The TV has one window, so it takes every link at once.
    func testTheTVsOneWindowTakesALinkAtOnce() {
        XCTAssertEqual(decide(.link(.tv), front: true), .runHere)
        XCTAssertEqual(decide(.link(.tv), front: false), .runHere)
    }

    /// iPadOS and visionOS: the first window subscribed takes a link, in front or not.
    func testAnyIPadOrVisionWindowTakesALinkAtOnce() {
        XCTAssertEqual(decide(.link(.scenes), front: true), .runHere)
        XCTAssertEqual(decide(.link(.scenes), front: false), .runHere)
    }

    /// A host page's connect or pairing needs an idle window: a busy one leaves it, in front or
    /// not.
    func testABusyWindowLeavesAHostPageDial() {
        for front in [true, false] {
            XCTAssertEqual(decide(.hostDial, busy: true, front: front), .leave)
            XCTAssertEqual(decide(.hostDial, busy: false, front: front), .runHere)
        }
    }

    /// A host page's browse or wake runs in any window.
    func testAnyWindowRunsAHostPageBrowseOrWake() {
        XCTAssertEqual(decide(.hostOther, busy: true, front: false), .runHere)
    }
}
