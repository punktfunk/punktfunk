// One library load, shared by the touch grid and the console shelf: the cached catalog first,
// a wake for a sleeping host, the host's catalog with retries while it boots, then what it runs.
// Each shell draws the events its own way.

import Foundation

/// One step of a library load, in the order `LibraryLoad` yields them.
public enum LibraryLoadEvent: Sendable {
    /// The catalog as last seen, launchers first, before the host is asked.
    case cached(CachedLibrary)
    /// A Wake-on-LAN packet went out; the fetch retries while the host boots.
    case waking
    /// The host's catalog, launchers first. The cache holds it too.
    case fetched([GameEntry])
    /// The catalog fetch gave up. `hadCache`: a cached catalog is up and stays.
    case failed(LibraryError, hadCache: Bool)
    /// What the host runs and downloads, and this device's grants there. Always the last event.
    case status(games: [RunningGame], downloads: [HostDownload], grants: UInt32?)
}

/// The load's sequence over its host. `run` builds one for a real host; a test fakes the calls.
public struct LibraryLoad {
    /// A woken box takes 20–60 s to answer: this many fetches, `retryEvery` apart.
    static let wakeAttempts = 12

    var hostID: String
    var cache: LibraryCache?
    /// Sends the wake; nil asks the host once.
    var wake: (() -> Void)?
    var fetch: () async throws -> [GameEntry]
    var status: () async -> (games: [RunningGame], downloads: [HostDownload], grants: UInt32?)
    var retryEvery: UInt64 = 5 * NSEC_PER_SEC

    /// The line a screen with no headline of its own shows for a failed load.
    public static func failure(_ error: LibraryError) -> String {
        "Couldn't load the library — \(error.errorDescription ?? "")"
    }

    /// Load `target`'s library, cached under `hostID`. With `autoWake` (the player's setting) on
    /// and `wakeMacs` known, it wakes the host first and retries an unreachable fetch while it
    /// boots; otherwise it asks once. Cancelling the consumer cancels the load.
    public static func run(
        target: MgmtTarget, hostID: String, wakeMacs: [String], autoWake: Bool
    ) -> AsyncStream<LibraryLoadEvent> {
        // Sent on every open, not only when the host looks offline: an awake machine ignores a
        // magic packet, and finding out first costs more than sending it.
        let wake: (() -> Void)? =
            !autoWake || wakeMacs.isEmpty || !PunktfunkConnection.wakeOnLANAvailable
            ? nil
            : {
                DispatchQueue.global(qos: .userInitiated).async { // blocking sends
                    PunktfunkConnection.wakeOnLAN(macs: wakeMacs, lastKnownIP: target.address)
                }
            }
        let load = LibraryLoad(
            hostID: hostID, cache: LibraryCache.shared, wake: wake,
            fetch: { try await LibraryClient.fetch(target) },
            status: { await LibraryClient.status(target) })
        return AsyncStream { continuation in
            let task = Task {
                await load.events { continuation.yield($0) }
                continuation.finish()
            }
            continuation.onTermination = { _ in task.cancel() }
        }
    }

    /// The whole load, each step handed to `emit`. Stops quietly once cancelled.
    func events(_ emit: (LibraryLoadEvent) -> Void) async {
        var cached = await cache?.load(hostID: hostID)
        if var shelf = cached {
            shelf.games = shelf.games.launchersFirst
            cached = shelf
            emit(.cached(shelf))
        }
        if let wake {
            wake()
            emit(.waking)
        }
        let attempts = wake == nil ? 1 : Self.wakeAttempts
        for attempt in 0..<attempts {
            if Task.isCancelled { return }
            do {
                let games = try await fetch().launchersFirst
                emit(.fetched(games))
                await cache?.store(games, hostID: hostID)
                break
            } catch {
                if Task.isCancelled { return }
                let failure = error as? LibraryError ?? .unreachable(error.localizedDescription)
                // Only "can't reach it" is worth waiting out: a refused certificate stays refused.
                guard case .unreachable = failure, attempt + 1 < attempts else {
                    emit(.failed(failure, hadCache: cached != nil))
                    break
                }
                try? await Task.sleep(nanoseconds: retryEvery)
            }
        }
        if Task.isCancelled { return }
        let answer = await status()
        emit(.status(games: answer.games, downloads: answer.downloads, grants: answer.grants))
    }
}
