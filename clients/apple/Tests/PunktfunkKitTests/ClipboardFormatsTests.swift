// The shared clipboard's format decisions — what a given pasteboard gets announced as, and what a
// host offer gets placed as. Pure functions, and the half of the sync that macOS and iOS
// share, so a change that suits one platform and breaks the other fails here.

import XCTest

@testable import PunktfunkKit

#if !os(tvOS)
final class ClipboardFormatsTests: XCTestCase {
    // MARK: - Announcing what is on the local pasteboard

    func testPlainTextAnnouncesOnlyText() {
        let kinds = ClipboardFormats.offerKinds(forTypes: ["public.utf8-plain-text"])
        XCTAssertEqual(kinds.map(\.mime), ["text/plain;charset=utf-8"])
    }

    func testRichTextAnnouncesEveryRepresentationInTableOrder() {
        // A copy out of a word processor leaves all three; the host picks the richest it can place,
        // so the order the table declares is the order the offer carries.
        let kinds = ClipboardFormats.offerKinds(forTypes: [
            "public.html", "public.utf8-plain-text", "public.rtf",
        ])
        XCTAssertEqual(kinds.map(\.mime), ["text/plain;charset=utf-8", "text/rtf", "text/html"])
    }

    func testUnknownTypesAreNotAnnounced() {
        // A pasteboard holding only things we have no wire vocabulary for announces nothing, which
        // legitimately clears the host's side rather than offering something unfetchable.
        let kinds = ClipboardFormats.offerKinds(forTypes: ["com.apple.mail.PasteboardTypeMessage"])
        XCTAssertTrue(kinds.isEmpty)
    }

    // MARK: - The PNG floor

    func testScreenshotTiffAnnouncesPngEvenThoughTiffIsNotOnTheWire() {
        // Screenshots and Preview leave TIFF, which no host can place. The adapters transcode at
        // fetch time, so announcing the PNG floor costs nothing until someone actually pastes.
        let kinds = ClipboardFormats.offerKinds(forTypes: ["public.tiff"])
        XCTAssertEqual(kinds.map(\.mime), ["image/png"])
    }

    func testPhotoHeicAnnouncesPng() {
        // The iOS case: a photo copied out of Photos is HEIC.
        let kinds = ClipboardFormats.offerKinds(forTypes: ["public.heic"])
        XCTAssertEqual(kinds.map(\.mime), ["image/png"])
    }

    func testJpegCrossesVerbatimAndStillCarriesThePngFloor() {
        // The original rides beside the floor rather than replacing it — a copied JPEG must not
        // balloon into a lossless PNG for peers that can take JPEG.
        let kinds = ClipboardFormats.offerKinds(forTypes: ["public.jpeg"])
        XCTAssertEqual(kinds.map(\.mime), ["image/jpeg", "image/png"])
    }

    func testGifCrossesVerbatimSoAnimationSurvives() {
        let kinds = ClipboardFormats.offerKinds(forTypes: ["com.compuserve.gif"])
        XCTAssertEqual(kinds.map(\.mime), ["image/gif", "image/png"])
    }

    func testNativePngIsNotAnnouncedTwice() {
        let kinds = ClipboardFormats.offerKinds(forTypes: ["public.png", "public.tiff"])
        XCTAssertEqual(kinds.map(\.mime), ["image/png"])
    }

    // MARK: - Secrets

    func testConcealedAndTransientPasteboardsAreRecognized() {
        // Password managers mark secrets with these; nothing so marked is ever announced.
        XCTAssertTrue(
            ClipboardFormats.isConcealed([
                "public.utf8-plain-text", "org.nspasteboard.ConcealedType",
            ]))
        XCTAssertTrue(
            ClipboardFormats.isConcealed([
                "public.utf8-plain-text", "org.nspasteboard.TransientType",
            ]))
        XCTAssertFalse(ClipboardFormats.isConcealed(["public.utf8-plain-text"]))
    }

    func testAConcealedPasteboardOffersNothingRatherThanAnEmptyOffer() {
        // The distinction matters: nil means "say nothing at all", where an empty list would tell
        // the host to drop what it has.
        let secret = StubPasteboard(types: ["public.utf8-plain-text", "org.nspasteboard.ConcealedType"])
        XCTAssertNil(secret.offerKinds)
        let ordinary = StubPasteboard(types: ["public.utf8-plain-text"])
        XCTAssertEqual(ordinary.offerKinds?.map(\.mime), ["text/plain;charset=utf-8"])
        let empty = StubPasteboard(types: [])
        XCTAssertEqual(empty.offerKinds?.isEmpty, true)
    }

    // MARK: - Placing what the host offered

    func testHostOfferMapsToUniformTypesAndSkipsWhatWeCannotPlace() {
        // Files ride Phase 2 — an offer carrying them places the rest and ignores that kind rather
        // than failing the whole paste.
        let kinds = [
            PunktfunkConnection.ClipKind(mime: "text/plain;charset=utf-8"),
            PunktfunkConnection.ClipKind(mime: "application/x-punktfunk-files"),
            PunktfunkConnection.ClipKind(mime: "image/png"),
        ]
        XCTAssertEqual(
            ClipboardFormats.placeableUtis(for: kinds), ["public.utf8-plain-text", "public.png"])
    }

    func testWireAndUniformTypeMapBothWays() {
        for (wire, uti) in ClipboardFormats.table {
            XCTAssertEqual(ClipboardFormats.uti(forWire: wire), uti)
            XCTAssertEqual(ClipboardFormats.wire(forUti: uti), wire)
        }
        XCTAssertNil(ClipboardFormats.uti(forWire: "application/x-punktfunk-files"))
        XCTAssertNil(ClipboardFormats.wire(forUti: "public.tiff"))
    }

    // MARK: - Handing bytes between threads

    func testResultBoxDeliversBytesToAWaiter() {
        let box = ClipboardResultBox()
        DispatchQueue.global().async { box.settle(Data("hello".utf8)) }
        XCTAssertEqual(box.wait(timeout: 5), Data("hello".utf8))
    }

    func testResultBoxTimesOutRatherThanWaitingForever() {
        // What a paste does when the host goes quiet mid-transfer: give up, insert nothing.
        XCTAssertNil(ClipboardResultBox().wait(timeout: 0.05))
    }

    func testResultBoxKeepsTheFirstAnswer() {
        // The fetch deadline and a late arrival can both fire; whichever settles first wins, and
        // the loser must not overwrite it or signal a second time.
        let box = ClipboardResultBox()
        box.settle(nil)
        box.settle(Data("late".utf8))
        XCTAssertNil(box.wait(timeout: 1))
    }
}

/// A pasteboard that holds nothing but a type list — enough to exercise the announce decision
/// without an AppKit or UIKit pasteboard in the test process.
private final class StubPasteboard: ClipboardPasteboard {
    private let types: [String]
    init(types: [String]) { self.types = types }

    var changeCount = 0
    var typeIdentifiers: [String] { types }
    func read(wire: String) -> Data? { nil }
    func installLazy(utis: [String], fetch: @escaping ClipboardFetch) -> Int { 0 }
    func installResolved(_ items: [(uti: String, data: Data)]) -> Int { 0 }
    func clear() -> Int { 0 }
    let resolvesPendingOfferOnTeardown = false
    func startObserving(onActivate: @escaping () -> Void) {}
    func stopObserving() {}
}
#endif
