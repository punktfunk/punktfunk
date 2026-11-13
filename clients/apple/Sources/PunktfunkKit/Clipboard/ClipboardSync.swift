// Shared clipboard, client half (design/clipboard-and-file-transfer.md §5.2). One implementation
// for macOS and iOS/iPadOS; everything that touches an actual pasteboard sits behind
// `ClipboardPasteboard`.
//
// Both directions are lazy:
//
// * **Local copy → host**: a changeCount poll announces the *format list* (`clipOffer`); the bytes
//   cross only when a host app pastes (a `.fetchRequest` event, answered from the live pasteboard
//   by `clipServe`).
// * **Host copy → local**: a `.remoteOffer` places a pasteboard item whose data provider fires only
//   when a local app actually pastes — the provider then pulls the bytes over a `clipFetch`.
//
// Password-manager respect: pasteboards marked `org.nspasteboard.ConcealedType` or
// `org.nspasteboard.TransientType` are never announced, never fetchable. Echo suppression: the
// changeCount of every write WE make is recorded so the announce poll skips it (§3.4).
//
// Phase 1 formats only (text / RTF / HTML / PNG / JPEG / GIF). Files ride Phase 2.
#if !os(tvOS)
import Foundation
import os

/// One live session's clipboard bridge. Created by the session model when streaming begins on a
/// host that advertises `HOST_CAP_CLIPBOARD` and whose per-host toggle is on; `stop()` before the
/// connection closes. All wire traffic runs on one dedicated drain thread, plus the OS-owned
/// threads that fulfil a paste.
public final class ClipboardSync: NSObject {
    /// How long a paste waits for the host's bytes before giving up and providing nothing (§5.2).
    /// Enforced here, so an adapter that has to block a thread can treat it as a guarantee.
    static let fetchTimeout: TimeInterval = 10
    /// Serve chunk size for host-side pastes of our data (bounds the per-call ABI copy).
    private static let serveChunk = 4 << 20
    /// Announce poll interval — how stale a local copy may be before the host hears about it.
    private static let announceInterval: TimeInterval = 0.5
    /// Ceiling on what `resolvePendingOffer` will pull. Text and modest images are worth having on
    /// the chance the user pastes them after the session ends; a 200 MB screenshot is not.
    private static let resolveBudget = 8 << 20
    /// Hard ceiling on ANY single transfer, either direction. Without it an inbound transfer grew
    /// a `Data` for whatever the host streamed, bounded only by the fetch deadline, and an
    /// outbound serve read the whole pasteboard however large it was. The resolve budget above is
    /// a policy about what is worth KEEPING past a session; this is the memory bound.
    private static let maxTransferBytes = 64 << 20
    /// And how long that may hold up teardown. Short on purpose — it sits between the user
    /// leaving and the connection closing, and a LAN round-trip for a few KB of text is
    /// milliseconds. An offer that cannot be had in this long is one the user does without.
    private static let resolveTimeout: TimeInterval = 3

    private let connection: PunktfunkConnection
    private let pasteboard: any ClipboardPasteboard
    /// `CLIP_FLAG_*` sent with the enable (`CLIP_FLAG_FILES` when the session permits files —
    /// always 0 in Phase 1).
    private let controlFlags: UInt8

    /// Host `.state` updates, delivered on the main queue — drives the toggle/footnote UI.
    public var onState: ((_ enabled: Bool, _ policy: UInt8, _ reason: UInt8) -> Void)?

    // MARK: Offer bookkeeping
    //
    // Read by the drain thread, and written by the thread tearing the sync down too, so it is all
    // under one lock. Nothing here is held across a fetch or a pasteboard read.
    private let stateLock = NSLock()
    /// The changeCount of the last host offer ANY session wrote. Another window's host clipboard
    /// is not a local copy, so no session announces it to its own host.
    private static let hostWrite = OSAllocatedUnfairLock(initialState: -1)
    private var offerSeq: UInt32 = 0
    private var lastSeenChangeCount = 0
    /// The changeCount of the last pasteboard write WE made (echo suppression, and "do we still
    /// own the pasteboard" on teardown).
    private var ownedChangeCount = -1
    /// The host offer currently placed on the local pasteboard (nil = none).
    private var installedRemote: (seq: UInt32, kinds: [PunktfunkConnection.ClipKind])?
    /// The offer already pulled down to concrete bytes, so teardown neither re-fetches it nor
    /// takes it back off the pasteboard.
    private var resolvedSeq: UInt32?

    // MARK: Outbound fetches
    //
    // Appended by whichever thread starts a fetch, completed by the drain thread as `.data`
    // arrives. Guarded by `fetchLock`, which is never held while a completion runs.
    private final class PendingFetch {
        var buffer = Data()
        let completion: (Data?) -> Void
        init(completion: @escaping (Data?) -> Void) { self.completion = completion }
    }
    private let fetchLock = NSLock()
    private var pendingFetches: [UInt32: PendingFetch] = [:]
    /// Fires the deadline that keeps a blocked paste from waiting forever on a host that went
    /// quiet mid-transfer.
    private let deadlines = DispatchQueue(label: "io.unom.punktfunk.clipboard.deadline")
    /// Serves host pastes off the drain thread where a pasteboard read can await a user decision
    /// (iOS's paste permission alert) — the drain thread must keep running, it is the one that
    /// would deliver the host's cancel.
    private let serves = DispatchQueue(label: "io.unom.punktfunk.clipboard.serve")

    private final class Flag: @unchecked Sendable {
        private let lock = NSLock()
        private var value = false
        func raise() {
            lock.lock()
            value = true
            lock.unlock()
        }
        var isRaised: Bool {
            lock.lock()
            defer { lock.unlock() }
            return value
        }
        /// Read-and-clear, for the one-shot "check the pasteboard now" nudge.
        func take() -> Bool {
            lock.lock()
            defer { lock.unlock() }
            let was = value
            value = false
            return was
        }
    }
    private let stopped = Flag()
    /// Raised by the activation observer, taken by the drain loop: the user may have copied
    /// elsewhere and is coming back to paste — announce now rather than waiting out the poll.
    private let checkNow = Flag()
    private let drainDone = DispatchSemaphore(value: 0)
    private var started = false

    /// - Parameter allowFiles: reserved for Phase 2; `CLIP_FLAG_FILES` is never set yet.
    public convenience init(connection: PunktfunkConnection, allowFiles: Bool = false) {
        self.init(connection: connection, pasteboard: SystemPasteboard(), allowFiles: allowFiles)
    }

    /// Designated init, taking the pasteboard so tests can drive the whole state machine against a
    /// stub without an AppKit/UIKit pasteboard in the way.
    init(
        connection: PunktfunkConnection, pasteboard: any ClipboardPasteboard,
        allowFiles: Bool = false
    ) {
        self.connection = connection
        self.pasteboard = pasteboard
        self.controlFlags = 0 // CLIP_FLAG_FILES rides Phase 2
        _ = allowFiles
        super.init()
    }

    // MARK: - Lifecycle

    /// Enable sync with the host and start the drain thread. The host answers the enable with a
    /// `.state` event (surfaced via `onState`) — `BACKEND_UNAVAILABLE` et al. arrive there.
    public func start() {
        guard !started else { return }
        started = true
        connection.clipControl(enabled: true, flags: controlFlags)
        // Baseline: whatever is on the pasteboard when sync starts is announced immediately — the
        // "copy first, then connect and paste" flow must work.
        stateLock.lock()
        lastSeenChangeCount = -1
        stateLock.unlock()
        pasteboard.startObserving(onActivate: { [checkNow] in checkNow.raise() })
        let thread = Thread { [weak self] in self?.drain() }
        thread.name = "punktfunk-clipboard"
        thread.qualityOfService = .utility
        thread.start()
    }

    /// Disable sync and join the drain thread. Called off-main before `connection.close()` (the
    /// same discipline as the audio/feedback drains). The drain holds the sync for its loop, so
    /// only this or the connection closing ends it: dropping the sync does not.
    ///
    /// A host offer still sitting on the local pasteboard as a promise has to be dealt with here,
    /// because after this returns nothing can answer it: either it is pulled down to real bytes
    /// (iOS, where this teardown is the user walking away with something they copied) or it is
    /// cleared, so a later paste comes up empty rather than silently doing nothing.
    public func stop() {
        guard started else { return }
        started = false
        pasteboard.stopObserving()
        // Before anything is torn down — the drain thread has to still be running to deliver the
        // chunks, and the connection still open to carry the fetch.
        if pasteboard.resolvesPendingOfferOnTeardown {
            resolvePendingOffer()
        }
        connection.clipControl(enabled: false, flags: 0)
        stopped.raise()
        drainDone.wait()
        // Fail every paste still blocked on us so nothing waits out its timeout against a dead
        // session.
        settleAll(nil)
        stateLock.lock()
        let ownsUnresolvedOffer =
            installedRemote != nil && resolvedSeq != installedRemote?.seq
            && pasteboard.changeCount == ownedChangeCount
        installedRemote = nil
        stateLock.unlock()
        if ownsUnresolvedOffer {
            _ = pasteboard.clear()
        }
    }

    private func drain() {
        var lastAnnounceCheck = -Double.infinity // monotonic: a clock step can't stall the poll
        while !stopped.isRaised {
            // Drain events (bounded burst so a chatty host can't starve the announce poll).
            var drained = 0
            while drained < 32, !stopped.isRaised {
                let ev: PunktfunkConnection.ClipEvent?
                do {
                    ev = try connection.nextClipboard(timeoutMs: drained == 0 ? 200 : 0)
                } catch {
                    stopped.raise() // session closed
                    break
                }
                guard let ev else { break }
                drained += 1
                handle(ev)
            }
            let now = ProcessInfo.processInfo.systemUptime
            if now - lastAnnounceCheck >= Self.announceInterval
                || checkNow.take()
            {
                lastAnnounceCheck = now
                announceIfChanged()
            }
        }
        drainDone.signal()
    }

    // MARK: - Local copy → host (announce)

    /// Announce the local pasteboard's format list when it changed, skipping our own writes and
    /// concealed/transient pasteboards. Runs on the drain thread.
    private func announceIfChanged() {
        let count = pasteboard.changeCount
        stateLock.lock()
        guard count != lastSeenChangeCount else {
            stateLock.unlock()
            return
        }
        lastSeenChangeCount = count
        guard count != ownedChangeCount else {
            stateLock.unlock() // our own write (a remote offer) — never echo
            return
        }
        installedRemote = nil // a local copy, or another session's host, replaced the offer
        stateLock.unlock()
        guard count != Self.hostWrite.withLock({ $0 }) else { return }

        guard let kinds = pasteboard.offerKinds else { return } // concealed — never announced
        stateLock.lock()
        offerSeq &+= 1
        let seq = offerSeq
        stateLock.unlock()
        // Empty = the pasteboard holds nothing we sync (or was cleared) — clears the host side.
        connection.clipOffer(seq: seq, kinds: kinds)
    }

    // MARK: - Event handling (drain thread)

    private func handle(_ ev: PunktfunkConnection.ClipEvent) {
        switch ev {
        case let .state(enabled, policy, reason):
            if let onState {
                DispatchQueue.main.async { onState(enabled, policy, reason) }
            }
        case let .remoteOffer(seq, kinds):
            installRemoteOffer(seq: seq, kinds: kinds)
        case let .fetchRequest(reqId, seq, _, mime):
            serves.async { [weak self] in self?.serveFetch(reqId: reqId, seq: seq, mime: mime) }
        case let .data(xferId, chunk, last):
            fetchLock.lock()
            pendingFetches[xferId]?.buffer.append(chunk)
            let overrun = (pendingFetches[xferId]?.buffer.count ?? 0) > Self.maxTransferBytes
            // Past the ceiling this is not a clipboard item any more — drop it rather than let
            // the host decide how much of this device's memory to take.
            let finished = (last || overrun) ? pendingFetches.removeValue(forKey: xferId) : nil
            fetchLock.unlock()
            if overrun {
                connection.clipCancel(id: xferId)
                finished?.completion(nil)
                return
            }
            // Outside the lock: a completion may start the next fetch (or wake a thread that will).
            if let finished {
                finished.completion(finished.buffer)
            }
        case let .cancelled(id), let .error(id, _):
            settle(id, nil)
        }
    }

    // MARK: - Host copy → local (lazy placement + paste-time fetch)

    /// Place a pasteboard item advertising the host's formats, each backed by a lazy provider —
    /// bytes cross only when a local app pastes. Empty `kinds` = the host cleared its clipboard:
    /// drop our item if it's still current.
    private func installRemoteOffer(seq: UInt32, kinds: [PunktfunkConnection.ClipKind]) {
        let utis = ClipboardFormats.placeableUtis(for: kinds)
        guard !utis.isEmpty else {
            stateLock.lock()
            let owned = installedRemote != nil && pasteboard.changeCount == ownedChangeCount
            installedRemote = nil
            resolvedSeq = nil
            if owned {
                let after = pasteboard.clear()
                ownedChangeCount = after
                lastSeenChangeCount = after
                Self.hostWrite.withLock { $0 = after }
            }
            stateLock.unlock()
            return
        }
        let fetch: ClipboardFetch = { [weak self] wire, done in
            guard let self else {
                done(nil)
                return
            }
            self.fetch(seq: seq, wire: wire, completion: done)
        }
        let before = pasteboard.changeCount
        let after = pasteboard.installLazy(utis: utis, fetch: fetch)
        stateLock.lock()
        installedRemote = (seq, kinds)
        resolvedSeq = nil
        // Only claim the pasteboard if the write actually landed. Recording a change count we did
        // not cause is the one bookkeeping mistake with no recovery: the announce poll would read
        // the user's own next copy as our echo and never tell the host about it again.
        if after != before {
            ownedChangeCount = after
            lastSeenChangeCount = after
            Self.hostWrite.withLock { $0 = after }
        }
        stateLock.unlock()
    }

    /// Start pulling one wire format of host offer `seq`. `completion` runs exactly once — with
    /// the bytes, or with nil on a stale offer, a timeout, a cancel, or a closing session.
    private func fetch(seq: UInt32, wire: String, completion: @escaping (Data?) -> Void) {
        fetchLock.lock()
        guard !stopped.isRaised, let xferId = connection.clipFetch(seq: seq, mime: wire) else {
            fetchLock.unlock()
            completion(nil)
            return
        }
        pendingFetches[xferId] = PendingFetch(completion: completion)
        fetchLock.unlock()
        deadlines.asyncAfter(deadline: .now() + Self.fetchTimeout) { [weak self] in
            guard let self, self.settle(xferId, nil) else { return }
            self.connection.clipCancel(id: xferId)
        }
    }

    /// Complete one pending fetch if it hasn't been already. Returns whether this call is the one
    /// that settled it, so a deadline knows whether it still has to cancel the transfer.
    @discardableResult
    private func settle(_ xferId: UInt32, _ data: Data?) -> Bool {
        fetchLock.lock()
        let pending = pendingFetches.removeValue(forKey: xferId)
        fetchLock.unlock()
        pending?.completion(data)
        return pending != nil
    }

    private func settleAll(_ data: Data?) {
        fetchLock.lock()
        let all = pendingFetches
        pendingFetches.removeAll()
        fetchLock.unlock()
        for (_, pending) in all {
            pending.completion(data)
        }
    }

    // MARK: - Resolving a promise before it dies (iOS)

    /// Pull the host's offer down to concrete bytes, replacing the promise on the pasteboard.
    ///
    /// A lazy promise is only as good as the ability to answer it, and on iOS that ability ends
    /// with the session, which backgrounding ends (`BackgroundPolicy`). Without this, "copy on the
    /// host, then paste into Safari on the iPad" would hand Safari an empty promise. Everything else
    /// about the design stays lazy — a user who copies on the host, pastes nothing, and stays in
    /// the app moves no clipboard bytes at all; this runs once, at the end, for content that is
    /// still on the pasteboard and still unclaimed.
    ///
    /// Runs on the thread calling `stop()` — off-main by contract, and never the drain thread,
    /// which has to keep running to deliver what this waits for.
    private func resolvePendingOffer() {
        stateLock.lock()
        let offer = installedRemote
        let unresolved = resolvedSeq != installedRemote?.seq
        let stillOurs = pasteboard.changeCount == ownedChangeCount
        stateLock.unlock()
        guard let offer, unresolved, stillOurs, !stopped.isRaised else { return }

        let deadline = Date().addingTimeInterval(Self.resolveTimeout)
        var items: [(uti: String, data: Data)] = []
        var budget = Self.resolveBudget
        for kind in offer.kinds {
            guard let uti = ClipboardFormats.uti(forWire: kind.mime) else { continue }
            // A size hint of 0 means "unknown" — try it, and let the byte count enforce the cap.
            guard kind.sizeHint <= UInt64(budget), !stopped.isRaised else { continue }
            let left = deadline.timeIntervalSinceNow
            guard left > 0 else { break }
            let box = ClipboardResultBox()
            fetch(seq: offer.seq, wire: kind.mime) { box.settle($0) }
            guard let data = box.wait(timeout: left), data.count <= budget else { continue }
            budget -= data.count
            items.append((uti, data))
        }
        guard !items.isEmpty else { return }
        // Re-check: the host may have copied again, or the user may have copied locally, while we
        // were pulling — either way these bytes are no longer what belongs on the pasteboard.
        stateLock.lock()
        defer { stateLock.unlock() }
        let before = pasteboard.changeCount
        guard installedRemote?.seq == offer.seq, before == ownedChangeCount else { return }
        let after = pasteboard.installResolved(items)
        // As in `installRemoteOffer`: a write that did not land leaves the promise in place, so
        // teardown should still take it back rather than believing these bytes are on the board.
        guard after != before else { return }
        resolvedSeq = offer.seq
        ownedChangeCount = after
        lastSeenChangeCount = after
        Self.hostWrite.withLock { $0 = after }
    }

    // MARK: - Host paste of our data (serve)

    /// Answer a host paste of our offered data from the live pasteboard. A stale `seq` (the local
    /// clipboard changed since that announce) is cancelled — never serve mismatched bytes.
    ///
    /// Runs on `serves`, not the drain thread: reading the pasteboard can block on a user decision
    /// (iOS's paste permission alert), and the drain thread has to stay live throughout. The
    /// pasteboard and the session can both change under that wait, so both are checked again
    /// once the read returns.
    private func serveFetch(reqId: UInt32, seq: UInt32, mime: String) {
        stateLock.lock()
        let count = pasteboard.changeCount
        // `installedRemote == nil` matters: installing the HOST's offer on our own pasteboard
        // updates the change count too, so without it a fetch for our promise would be served by
        // reading that promise — which fetches back from the host and blocks this queue on it.
        let fresh = seq == offerSeq && count == lastSeenChangeCount && installedRemote == nil
        stateLock.unlock()
        guard fresh, !stopped.isRaised,
              let data = pasteboard.read(wire: mime, unchangedSince: count),
              !stopped.isRaised, data.count <= Self.maxTransferBytes
        else {
            connection.clipCancel(id: reqId)
            return
        }
        var offset = 0
        while offset < data.count {
            let end = min(offset + Self.serveChunk, data.count)
            connection.clipServe(
                reqId: reqId, data: data.subdata(in: offset..<end), last: end == data.count)
            offset = end
        }
        if data.isEmpty {
            connection.clipServe(reqId: reqId, data: Data(), last: true)
        }
    }
}
#endif
