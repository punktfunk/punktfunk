// The iOS/iPadOS half of the clipboard seam: `UIPasteboard.general`.
//
// Two things differ from AppKit in ways that shape the code here.
//
// **Laziness is asynchronous.** UIKit promises data with `NSItemProvider`, whose load handler is
// handed a completion rather than a return value, so this adapter passes the fetch straight
// through — no thread is blocked waiting for a paste to resolve.
//
// **Reading the pasteboard is a privacy event.** Since iOS 14 the system tells the user when an
// app reads pasteboard *contents*, and since iOS 16 it asks first when the content came from
// another app. Reading *metadata* — the change count, the list of type identifiers — does not.
// That maps exactly onto the lazy design: the announce poll only ever looks at metadata, so it is
// silent no matter how long a session runs, and the one moment a read really happens is when
// someone on the host pastes, which is a deliberate act the user is present for.
#if !os(tvOS) && !os(macOS) && canImport(UIKit)
import Foundation
import UIKit
import UniformTypeIdentifiers

typealias SystemPasteboard = UIKitPasteboard

final class UIKitPasteboard: ClipboardPasteboard {
    private let pb = UIPasteboard.general
    private var activationObserver: NSObjectProtocol?

    var changeCount: Int { pb.changeCount }

    /// `types` reports the identifiers present without touching a single byte of content, so the
    /// poll costs the user nothing and raises no banner.
    var typeIdentifiers: [String] { pb.types }

    /// Read one wire format, converting where iOS stores a different native type: `image/png` is
    /// served from a real PNG entry when present, else re-encoded from whatever image the
    /// pasteboard holds — a photo copied out of Photos is HEIC, a screenshot may arrive as TIFF,
    /// and neither is something a host can be expected to place.
    ///
    /// This is the one call that reads contents, and on iOS 16+ it can put a permission alert in
    /// front of the user and wait for their answer. `ClipboardSync` calls it off the drain thread
    /// for exactly that reason.
    func read(wire: String) -> Data? {
        guard wire == "image/png" else {
            guard let uti = ClipboardFormats.uti(forWire: wire) else { return nil }
            if let data = pb.data(forPasteboardType: uti) {
                return data
            }
            // UIPasteboard stores plain text as a string rather than a data representation often
            // enough that the typed read comes back empty on content we can plainly see.
            guard uti == UTType.utf8PlainText.identifier else { return nil }
            return pb.string?.data(using: .utf8)
        }
        if let png = pb.data(forPasteboardType: UTType.png.identifier) {
            return png
        }
        return pb.image?.pngData()
    }

    func installLazy(utis: [String], fetch: @escaping ClipboardFetch) -> Int {
        let provider = NSItemProvider()
        for uti in utis {
            guard let type = UTType(uti), let wire = ClipboardFormats.wire(forUti: uti) else {
                continue
            }
            provider.registerDataRepresentation(for: type, visibility: .all) { completion in
                fetch(wire) { data in
                    if let data {
                        completion(data, nil)
                    } else {
                        completion(nil, ClipboardOfferError.unavailable)
                    }
                }
                return nil
            }
        }
        // `localOnly`: these bytes do not exist on this device yet — they are a promise against a
        // session that is about to end. Handing that to Universal Clipboard would either force an
        // eager pull of everything the host ever copies or strand another device with a promise
        // nothing can answer.
        pb.setItemProviders([provider], localOnly: true, expirationDate: nil)
        return pb.changeCount
    }

    func installResolved(_ items: [(uti: String, data: Data)]) -> Int {
        var representations: [String: Any] = [:]
        for (uti, data) in items {
            representations[uti] = data
        }
        pb.setItems([representations], options: [.localOnly: true])
        return pb.changeCount
    }

    func clear() -> Int {
        pb.items = []
        return pb.changeCount
    }

    /// Backgrounding the app ends the session, at once or when the keep-alive runs out
    /// (`BackgroundPolicy`), and with it any hope of answering a promise — so an offer the user has
    /// not pasted yet is pulled down to real bytes while the connection is still open.
    let resolvesPendingOfferOnTeardown = true

    func startObserving(onActivate: @escaping () -> Void) {
        activationObserver = NotificationCenter.default.addObserver(
            forName: UIApplication.didBecomeActiveNotification, object: nil, queue: nil
        ) { _ in onActivate() }
    }

    func stopObserving() {
        if let activationObserver {
            NotificationCenter.default.removeObserver(activationObserver)
            self.activationObserver = nil
        }
    }
}

/// What a lazy representation reports when the host cannot supply it — a stale offer, a timed-out
/// fetch, or a session that ended. UIKit shows the paste as producing nothing, which is the same
/// outcome AppKit gets by providing no data.
enum ClipboardOfferError: Error {
    case unavailable
}
#endif
