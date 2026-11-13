// The platform seam under `ClipboardSync`: everything that actually touches NSPasteboard or
// UIPasteboard, and nothing else.
//
// The sync logic above this protocol — the drain thread, the offer sequence numbers, the pending
// fetches a blocked paste waits on, echo suppression — is identical on macOS and iOS and is worth
// having exactly one copy of. What genuinely differs is small and lives in the two adapters:
// AppKit fulfils a paste by BLOCKING the thread it asks on, UIKit by answering an asynchronous
// load handler; AppKit transcodes images through NSImage, UIKit through UIImage; and only UIKit
// has to worry about the process being suspended out from under an offer it promised to serve.
#if !os(tvOS)
import Foundation

/// Pulls the bytes of one lazily-offered wire format from the host. Called on whichever thread the
/// OS fulfils a paste on — never the drain thread, which has to stay free to deliver the very
/// chunks this fetch is waiting for — and answers asynchronously.
typealias ClipboardFetch = (_ wire: String, _ completion: @escaping (Data?) -> Void) -> Void

/// The system pasteboard, as much of it as the shared clipboard needs.
///
/// Calls arrive from the drain thread, the serve queue and the thread starting or stopping the
/// sync, and `ClipboardSync` does not serialize them: the system pasteboards take calls from any
/// thread. The writes that touch an adapter's own state, `installLazy` and `clear`, come from the
/// drain thread or after it has joined. No call may block on the **main** queue: main can be
/// waiting in an AppKit paste for bytes only the drain thread delivers.
protocol ClipboardPasteboard: AnyObject {
    /// Monotonic per pasteboard write, by anyone. Reading it must never count as reading the
    /// pasteboard's *contents* — on iOS that distinction is the difference between a silent poll
    /// and a system paste banner on every tick.
    var changeCount: Int { get }

    /// The uniform type identifiers currently on the pasteboard. Must be answerable WITHOUT
    /// reading contents, for the same reason.
    var typeIdentifiers: [String] { get }

    /// Bytes for one wire format, read from the live pasteboard and transcoded where the system
    /// stores a different native type (`image/png` from a TIFF screenshot). Nil when the format
    /// is not really there. This one DOES read contents.
    func read(wire: String) -> Data?

    /// Replace the pasteboard with a single item advertising `utis`, each backed by `fetch` — the
    /// bytes cross only if something actually pastes. Returns the resulting `changeCount`, which
    /// the caller records so it can tell its own write apart from the user's next copy.
    func installLazy(utis: [String], fetch: @escaping ClipboardFetch) -> Int

    /// Replace the pasteboard with concrete bytes. Only iOS needs this (see
    /// `ClipboardSync.resolvePendingOffer`); on macOS a lazy promise outlives any paste that
    /// might come, so the adapter there never has cause to call it.
    func installResolved(_ items: [(uti: String, data: Data)]) -> Int

    /// Empty the pasteboard. Returns the resulting `changeCount`.
    func clear() -> Int

    /// Whether a host offer still sitting unresolved on the pasteboard should be pulled down to
    /// concrete bytes as the sync is torn down, instead of being dropped.
    ///
    /// This is the difference between the two platforms' idea of how long a promise lives. A Mac
    /// keeps running long after a session ends, but the promise dies with the sync either way, so
    /// AppKit clears it and the user loses nothing they had before. On iOS the teardown IS the
    /// user leaving — backgrounding ends the session — and "copy on the host, then paste into
    /// Safari" is the whole point of the feature on a tablet, so those bytes have to be made real
    /// while the connection that can still supply them is open.
    var resolvesPendingOfferOnTeardown: Bool { get }

    /// Start watching for the user coming back to the app. The case that matters is "copied
    /// elsewhere, now focusing the stream to paste" — the offer must reach the host before their
    /// ⌘V lands, which is sooner than the announce poll would get there on its own.
    func startObserving(onActivate: @escaping () -> Void)
    func stopObserving()
}

/// A fetch result handed between threads: the OS fulfils a paste on one thread and the drain
/// thread produces the bytes on another.
final class ClipboardResultBox: @unchecked Sendable {
    private let ready = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var value: Data?
    private var settled = false

    func settle(_ data: Data?) {
        lock.lock()
        guard !settled else {
            lock.unlock()
            return
        }
        settled = true
        value = data
        lock.unlock()
        ready.signal()
    }

    /// Blocks until the bytes arrive, or gives up. Never call this from the drain thread — it is
    /// the drain thread that delivers what is being waited for.
    func wait(timeout: TimeInterval) -> Data? {
        guard ready.wait(timeout: .now() + timeout) == .success else { return nil }
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

extension ClipboardPasteboard {
    /// The wire formats this pasteboard is currently carrying, honouring the concealed/transient
    /// markers. Nil when the pasteboard holds a secret — distinct from "holds nothing we sync",
    /// which is an empty list and legitimately clears the peer.
    var offerKinds: [PunktfunkConnection.ClipKind]? {
        let types = typeIdentifiers
        guard !ClipboardFormats.isConcealed(types) else { return nil }
        return ClipboardFormats.offerKinds(forTypes: types)
    }

    /// `read(wire:)`, or nil when the pasteboard is no longer at `count` once the read returns.
    /// The read can wait on a user decision (iOS's paste alert), and what it then returns is
    /// whatever is on the pasteboard by that time: a newer copy, even a concealed one.
    func read(wire: String, unchangedSince count: Int) -> Data? {
        guard let data = read(wire: wire), changeCount == count else { return nil }
        return data
    }
}
#endif
