// The macOS half of the clipboard seam: `NSPasteboard.general`.
//
// AppKit's lazy-paste contract is a blocking one — `provideDataForType` is called the moment a
// Mac app pastes, on this app's main thread when the paste comes from another app (macOS 27), and
// whatever the item holds when that call returns is what the app gets. So this adapter is the one
// place that turns the asynchronous fetch into a wait.
#if os(macOS)
import AppKit
import Foundation

typealias SystemPasteboard = AppKitPasteboard

final class AppKitPasteboard: ClipboardPasteboard {
    private let pb = NSPasteboard.general
    private var activationObserver: NSObjectProtocol?
    /// The provider backing the offer currently on the pasteboard. AppKit's own reference to it is
    /// not something to rely on: nothing crosses if it is collected before the user pastes.
    private var provider: BlockingOfferProvider?

    var changeCount: Int { pb.changeCount }

    var typeIdentifiers: [String] { (pb.types ?? []).map(\.rawValue) }

    /// Read one wire format, converting where macOS stores a different native type: `image/png` is
    /// served from a real `.png` entry when present, else converted from whatever image
    /// representation the pasteboard holds (TIFF from screenshots and Preview, WebP/AVIF/GIF from
    /// browsers — `NSImage` decodes them all) into PNG at fetch time.
    func read(wire: String) -> Data? {
        guard wire == "image/png" else {
            guard let uti = ClipboardFormats.uti(forWire: wire) else { return nil }
            return pb.data(forType: NSPasteboard.PasteboardType(uti))
        }
        if let png = pb.data(forType: .png) {
            return png
        }
        guard let img = NSImage(pasteboard: pb),
            let tiff = img.tiffRepresentation,
            let rep = NSBitmapImageRep(data: tiff)
        else {
            return nil
        }
        return rep.representation(using: .png, properties: [:])
    }

    func installLazy(utis: [String], fetch: @escaping ClipboardFetch) -> Int {
        let provider = BlockingOfferProvider(fetch: fetch)
        let item = NSPasteboardItem()
        item.setDataProvider(provider, forTypes: utis.map { NSPasteboard.PasteboardType($0) })
        pb.clearContents()
        pb.writeObjects([item])
        self.provider = provider
        return pb.changeCount
    }

    /// Unused on macOS — a promise here outlives any paste that might come, so there is never
    /// cause to resolve one early. Implemented anyway so the seam has no platform-shaped hole.
    func installResolved(_ items: [(uti: String, data: Data)]) -> Int {
        let item = NSPasteboardItem()
        for (uti, data) in items {
            item.setData(data, forType: NSPasteboard.PasteboardType(uti))
        }
        pb.clearContents()
        pb.writeObjects([item])
        provider = nil
        return pb.changeCount
    }

    func clear() -> Int {
        pb.clearContents()
        provider = nil
        return pb.changeCount
    }

    /// A Mac keeps running after a session ends, but the promise dies with the sync regardless, so
    /// there is nothing to be gained by spending a round-trip on it at teardown — clearing leaves
    /// the user exactly where they were.
    let resolvesPendingOfferOnTeardown = false

    func startObserving(onActivate: @escaping () -> Void) {
        activationObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification, object: nil, queue: nil
        ) { _ in onActivate() }
    }

    func stopObserving() {
        if let activationObserver {
            NotificationCenter.default.removeObserver(activationObserver)
            self.activationObserver = nil
        }
    }
}

/// The lazy paste hook: AppKit calls `provideDataForType` only when a Mac app actually pastes; the
/// fetch then blocks the calling thread, main for another app's paste, until the host's bytes
/// arrive or `ClipboardSync.fetchTimeout` passes. On timeout or a dead session it provides
/// nothing, so the paste inserts nothing rather than hanging.
private final class BlockingOfferProvider: NSObject, NSPasteboardItemDataProvider {
    private let fetch: ClipboardFetch

    init(fetch: @escaping ClipboardFetch) {
        self.fetch = fetch
    }

    func pasteboard(
        _ pasteboard: NSPasteboard?, item: NSPasteboardItem,
        provideDataForType type: NSPasteboard.PasteboardType
    ) {
        guard let wire = ClipboardFormats.wire(forUti: type.rawValue) else { return }
        let box = ClipboardResultBox()
        fetch(wire) { box.settle($0) }
        // The fetch enforces its own deadline and always completes; this is only a backstop
        // against a lost completion wedging an AppKit thread forever.
        guard let data = box.wait(timeout: ClipboardSync.fetchTimeout + 2) else { return }
        item.setData(data, forType: type)
    }
}
#endif
