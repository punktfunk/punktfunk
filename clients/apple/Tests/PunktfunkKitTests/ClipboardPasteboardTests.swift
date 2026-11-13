import XCTest
@testable import PunktfunkKit

final class ClipboardPasteboardTests: XCTestCase {
    /// A pasteboard whose read takes a while, like iOS's paste alert: `copyDuringRead` lands
    /// while it waits, and the read then returns that newer content.
    private final class SlowPasteboard: ClipboardPasteboard {
        var changeCount = 7
        var content = "offered"
        var copyDuringRead: String?
        var typeIdentifiers: [String] { [] }
        func read(wire: String) -> Data? {
            if let newer = copyDuringRead {
                content = newer
                changeCount += 1
            }
            return Data(content.utf8)
        }
        func installLazy(utis: [String], fetch: @escaping ClipboardFetch) -> Int { changeCount }
        func installResolved(_ items: [(uti: String, data: Data)]) -> Int { changeCount }
        func clear() -> Int { changeCount }
        var resolvesPendingOfferOnTeardown: Bool { false }
        func startObserving(onActivate: @escaping () -> Void) {}
        func stopObserving() {}
    }

    func testAReadServesWhatWasOffered() {
        let pasteboard = SlowPasteboard()
        XCTAssertEqual(
            pasteboard.read(wire: "text/plain", unchangedSince: 7), Data("offered".utf8))
    }

    /// The host asked for the offer at change count 7. A copy that lands while the read waits
    /// on the paste alert is not that offer, and may be a password: nothing is served.
    func testACopyDuringTheReadServesNothing() {
        let pasteboard = SlowPasteboard()
        pasteboard.copyDuringRead = "a password"
        XCTAssertNil(pasteboard.read(wire: "text/plain", unchangedSince: 7))
    }
}
